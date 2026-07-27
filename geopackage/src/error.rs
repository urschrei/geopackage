//! Error type for the `geopackage` crate.

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Underlying SQLite error.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Spec-level error from `geopackage-core`.
    #[error(transparent)]
    Core(#[from] geopackage_core::Error),
    /// Underlying Arrow error (feature `arrow`).
    #[cfg(feature = "arrow")]
    #[error(transparent)]
    Arrow(#[from] arrow_schema::ArrowError),
    /// A stored value's storage class cannot go into the Arrow array its column
    /// maps to (feature `arrow`).
    ///
    /// The columnar counterpart of [`Self::ValueTypeMismatch`], reported
    /// separately because it names an Arrow type rather than a declared
    /// GeoPackage one.
    #[cfg(feature = "arrow")]
    #[error("column {column:?} maps to Arrow {expected} but holds a {found} value")]
    ArrowValueMismatch {
        /// The column that was read.
        column: String,
        /// The Arrow type the column maps to.
        expected: &'static str,
        /// The SQLite storage class actually found: one of `NULL`, `INTEGER`,
        /// `REAL`, `TEXT`, or `BLOB`.
        found: &'static str,
    },
    /// A schema field whose Arrow type the columnar reader has no builder for
    /// (feature `arrow`).
    ///
    /// Unreachable from a schema this crate derived; it exists so that adding a
    /// mapping without adding its builder is an error rather than a panic.
    #[cfg(feature = "arrow")]
    #[error("no columnar builder for Arrow type {data_type}")]
    UnsupportedArrowType {
        /// The Arrow type that has no builder.
        data_type: String,
    },
    /// The file is not identifiable as a GeoPackage.
    #[error(
        "not a GeoPackage: {reason} (application_id={application_id:#010x}, user_version={user_version})"
    )]
    NotAGeoPackage {
        /// Why identification failed.
        reason: &'static str,
        /// The file's `application_id` pragma.
        application_id: u32,
        /// The file's `user_version` pragma.
        user_version: u32,
    },
    /// `create` was asked to overwrite an existing non-empty file.
    #[error("refusing to create GeoPackage over existing non-empty file: {0}")]
    AlreadyExists(std::path::PathBuf),
    /// An EPSG code outside the vendored definition subset.
    #[error(
        "EPSG:{code} is not in the vendored definition subset; \
         supply the WKT yourself via GeoPackage::add_srs"
    )]
    UnknownEpsgCode {
        /// The requested EPSG code.
        code: i32,
    },
    /// A `gpkg_geometry_columns.geometry_type_name` value outside the
    /// spec vocabulary (Annex G).
    #[error("unknown geometry type name {name:?} for table {table_name:?}")]
    UnknownGeometryType {
        /// The table the row describes.
        table_name: String,
        /// The unrecognised type name as stored.
        name: String,
    },
    /// A `gpkg_geometry_columns.z` or `.m` value outside `0`/`1`/`2`.
    #[error("invalid {column} flag {value} in gpkg_geometry_columns for table {table_name:?}")]
    InvalidZmFlag {
        /// The table the row describes.
        table_name: String,
        /// Which column carried the bad value: `"z"` or `"m"`.
        column: &'static str,
        /// The value as stored.
        value: i64,
    },
    /// Introspection was asked for a table that does not exist (its
    /// `PRAGMA table_info` returned no rows).
    #[error("no such table: {table_name:?}")]
    NoSuchTable {
        /// The requested table name.
        table_name: String,
    },
    /// A column was requested that the table does not have.
    #[error("table {table_name:?} has no column {column_name:?}")]
    NoSuchColumn {
        /// The table that was queried.
        table_name: String,
        /// The requested column name.
        column_name: String,
    },
    /// A stored value's SQLite storage class is incompatible with the column's
    /// declared GeoPackage type; the value is surfaced rather than coerced.
    #[error("column {column:?} declared {declared:?} holds an incompatible {found} value")]
    ValueTypeMismatch {
        /// The column that was read.
        column: String,
        /// The column's declared GeoPackage type.
        declared: geopackage_core::types::ColumnType,
        /// The SQLite storage class actually found: one of `NULL`, `INTEGER`,
        /// `REAL`, `TEXT`, or `BLOB`.
        found: &'static str,
    },
    /// A `BOOLEAN` column holds an INTEGER other than `0` or `1`, read under
    /// [`StorageStrictness::Strict`](crate::StorageStrictness::Strict).
    ///
    /// The storage class is the right one, so this is not a
    /// [`Self::ValueTypeMismatch`]: the value itself is outside what the
    /// declared type permits. Lenient conversion, the default, reads any
    /// non-zero integer as `true` instead.
    #[error("column {column:?} declared BOOLEAN holds {value}, which is neither 0 nor 1")]
    NonBooleanInteger {
        /// The column that was read.
        column: String,
        /// The integer the column actually holds.
        value: i64,
    },
    /// A `DATE` or `DATETIME` column holds text that does not parse.
    #[error("column {column:?} holds invalid date/datetime text {text:?}")]
    InvalidDateTimeValue {
        /// The column that was read.
        column: String,
        /// The offending text as stored.
        text: String,
        /// The underlying parse error.
        #[source]
        source: geopackage_core::datetime::DateTimeError,
    },
    /// A geometry column was read through the value API, which handles only
    /// non-geometry columns; geometry is read through the feature API.
    #[error("column {column:?} is a geometry column and cannot be read as a Value")]
    GeometryValueUnsupported {
        /// The geometry column that was read.
        column: String,
    },
    /// A geometry blob's WKB type does not satisfy the column's declared
    /// `gpkg_geometry_columns` type (opt-in check; see
    /// `Layer::with_geometry_type_validation`).
    #[error(
        "geometry in {table_name:?}.{column_name:?} is {found} but the column is declared {declared}"
    )]
    GeometryTypeMismatch {
        /// The feature table.
        table_name: String,
        /// The geometry column.
        column_name: String,
        /// The declared `gpkg_geometry_columns` type.
        declared: geopackage_core::types::GeometryType,
        /// The WKB body's actual type.
        found: geopackage_core::types::GeometryType,
    },
    /// A tile pyramid that does not satisfy the spec's consistency rules
    /// (Requirements 45 to 53), or a tile payload that could not be read.
    ///
    /// Raised by [`crate::GeoPackage::create_tile_pyramid`] and by the tile
    /// write path. Reading an existing pyramid never validates it, so a file
    /// another implementation wrote opens whatever its matrices say.
    #[error(transparent)]
    Tile(#[from] geopackage_core::TileError),
    /// A tile pyramid whose `gpkg_tile_matrix_set` row is missing, so the
    /// extent its tiles are addressed against is unknown.
    #[error(
        "tile pyramid {table_name:?} has no gpkg_tile_matrix_set row, so its tiles cannot be located"
    )]
    NoTileMatrixSet {
        /// The pyramid that is missing its row.
        table_name: String,
    },
    /// A zoom level the pyramid does not declare a `gpkg_tile_matrix` row for,
    /// so it has no grid to address tiles against.
    #[error("tile pyramid {table_name:?} declares no zoom level {zoom_level}")]
    UnknownZoomLevel {
        /// The pyramid that was addressed.
        table_name: String,
        /// The zoom level that is not declared.
        zoom_level: i64,
    },
    /// A tile payload in an encoding no tile pyramid may hold.
    ///
    /// The base spec allows PNG and JPEG (Requirements 36 and 37), and the
    /// `gpkg_webp` extension allows WebP, which the write path registers as it
    /// goes. A TIFF belongs to the tiled gridded coverage extension, which this
    /// crate does not implement.
    #[error(
        "tile payload for {table_name:?} is {format:?}, which a tile pyramid may not hold: \
         PNG and JPEG need no extension, WebP registers gpkg_webp"
    )]
    TileFormatNotAllowed {
        /// The pyramid that was written to.
        table_name: String,
        /// The encoding the payload's header declared.
        format: geopackage_core::TileFormat,
    },
    /// A pyramid whose zoom levels do not step by factors of two was created
    /// without opting into the `gpkg_zoom_other` extension.
    #[error(
        "tile pyramid {table_name:?} has zoom levels that do not step by factors of two, \
         which needs the gpkg_zoom_other extension: opt in with TilePyramidBuilder::allow_zoom_other"
    )]
    ZoomOtherNotEnabled {
        /// The pyramid that was rejected.
        table_name: String,
    },
    /// A layer was requested by a name that is not present in `gpkg_contents`.
    #[error("no such layer: {table_name:?} is not registered in gpkg_contents")]
    NoSuchLayer {
        /// The requested layer name.
        table_name: String,
    },
    /// A layer was requested with the wrong accessor: its `gpkg_contents`
    /// `data_type` does not match the accessor used ([`crate::GeoPackage::layer`]
    /// expects `features`, [`crate::GeoPackage::attributes`] expects
    /// `attributes`).
    #[error("layer {table_name:?} has data_type {found:?}, not {expected:?}")]
    WrongDataType {
        /// The layer as named in `gpkg_contents`.
        table_name: String,
        /// The `data_type` the accessor requires.
        expected: &'static str,
        /// The `data_type` actually recorded in `gpkg_contents`.
        found: String,
    },
    /// A spatial (bounding-box) query was requested on a layer that has no
    /// geometry column (an attribute layer, or a feature table whose
    /// `gpkg_geometry_columns` row is missing).
    #[error("layer {table_name:?} has no geometry column; spatial queries are unavailable")]
    NoGeometryColumn {
        /// The layer that was queried.
        table_name: String,
    },
    /// A table was requested with a name beginning `gpkg_`, which the spec
    /// reserves for its own tables (Requirement 25).
    #[error("table name {table_name:?} is reserved: user table names must not begin with 'gpkg_'")]
    ReservedTablePrefix {
        /// The rejected table name.
        table_name: String,
    },
    /// A table was created with a name that a table or view already uses.
    #[error("table {table_name:?} already exists")]
    TableAlreadyExists {
        /// The clashing table name.
        table_name: String,
    },
    /// A layer was created referencing an `srs_id` absent from
    /// `gpkg_spatial_ref_sys`.
    #[error(
        "srs_id {srs_id} is not present in gpkg_spatial_ref_sys; \
         register it first with GeoPackage::add_srs or GeoPackage::add_epsg_srs"
    )]
    UnknownSrs {
        /// The unregistered spatial reference system identifier.
        srs_id: i32,
    },
    /// A layer was created with an extension (non-linear or abstract) geometry
    /// type. Writing those types is a later milestone.
    #[error(
        "geometry type {geometry_type} requires a gpkg_geom_<TYPE> extension; \
         writing extension geometry types is not yet supported"
    )]
    ExtensionGeometryUnsupported {
        /// The rejected geometry type.
        geometry_type: geopackage_core::types::GeometryType,
    },
    /// [`crate::GeoPackage::create_layer`] was called with a builder that has no
    /// geometry column (use [`crate::GeoPackage::create_attributes_table`] for a
    /// non-spatial table).
    #[error("cannot create feature layer {table_name:?}: the builder has no geometry column")]
    MissingGeometrySpec {
        /// The table the builder describes.
        table_name: String,
    },
    /// [`crate::GeoPackage::create_attributes_table`] was called with a builder
    /// that has a geometry column (use [`crate::GeoPackage::create_layer`] for a
    /// feature table).
    #[error("cannot create attributes table {table_name:?}: the builder has a geometry column")]
    UnexpectedGeometrySpec {
        /// The table the builder describes.
        table_name: String,
    },
    /// [`crate::Layer::extent`] measured a layer's extent but could not record
    /// it, for a reason that is not another connection holding a lock.
    ///
    /// The measurement succeeded and is carried here, so nothing is lost by the
    /// failure: the caller can use `extent` and decide what to make of the
    /// store being unwritable. Lock contention does not produce this, because a
    /// concurrent writer means the measurement is not one the crate could vouch
    /// for anyway; what does are the conditions that mean the store is broken
    /// or unwritable in a way that will not clear, such as an unwritable
    /// directory, a full disk, or an I/O error.
    #[error("measured the extent of table {table_name:?} but could not record it: {source}")]
    ExtentPersist {
        /// The table whose extent was measured.
        table_name: String,
        /// The measured extent, which is what [`crate::Layer::extent`] would
        /// have returned. `None` when the layer had nothing to measure.
        extent: Option<crate::BoundingBox>,
        /// Why the write failed. Boxed only to keep this variant from setting
        /// the size of every `Result` in the crate.
        source: Box<rusqlite::Error>,
    },
    /// [`crate::Feature::geometry`] was called on a row whose read did not
    /// select the geometry column.
    ///
    /// Distinct from `Ok(None)`, which means the row's geometry is NULL. This
    /// says the geometry was never read, because
    /// [`crate::Layer::with_columns`] did not name it or
    /// [`crate::Layer::without_geometry`] excluded it, and it is an error
    /// rather than an empty answer so the two cannot be confused.
    /// [`crate::Feature::has_geometry_column`] tests for it without erroring.
    #[error("this feature's read did not select the geometry column")]
    GeometryNotProjected,
    /// A partial update named the same column more than once.
    ///
    /// SQLite accepts a repeated assignment and applies the last one, so this
    /// is rejected rather than resolved: naming a column twice with different
    /// values is more likely to be a caller's mistake than an intention.
    #[error("update of table {table_name:?} names column {column_name:?} more than once")]
    DuplicateUpdateColumn {
        /// The table being written.
        table_name: String,
        /// The column named twice.
        column_name: String,
    },
    /// A write supplied a value slice whose length does not match the layer's
    /// non-geometry column count.
    #[error("table {table_name:?} expects {expected} value(s) per row, got {found}")]
    ValueCountMismatch {
        /// The table being written.
        table_name: String,
        /// The number of non-geometry columns.
        expected: usize,
        /// The number of values supplied.
        found: usize,
    },
    /// A spatial-index operation was requested on a layer whose table has no
    /// single-column primary key. The RTree triggers key the index on that
    /// column, so one is required to build, repair, or use the index.
    #[error("table {table_name:?} has no single-column primary key; a spatial index requires one")]
    NoPrimaryKey {
        /// The table that was queried.
        table_name: String,
    },
    /// [`crate::Layer::create_spatial_index`] was called on a layer whose
    /// geometry column already carries an RTree spatial index (its
    /// `rtree_<table>_<column>` virtual table already exists).
    #[error("table {table_name:?} column {column_name:?} already has a spatial index")]
    SpatialIndexExists {
        /// The feature table.
        table_name: String,
        /// The geometry column.
        column_name: String,
    },
    /// [`crate::Layer::repair_spatial_index`] was called on a layer that has no
    /// RTree triggers to repair. Build an index with
    /// [`crate::Layer::create_spatial_index`] first.
    #[error(
        "table {table_name:?} column {column_name:?} has no spatial index to repair; \
         create one with Layer::create_spatial_index"
    )]
    NoSpatialIndex {
        /// The feature table.
        table_name: String,
        /// The geometry column.
        column_name: String,
    },
    /// A written geometry's `z`/`m` presence violates the geometry column's
    /// declared constraint (`gpkg_geometry_columns.z` / `.m`).
    #[error(
        "geometry for {table_name:?}.{column:?} {verb} a {dimension} dimension, \
         but the column declares it {constraint:?}"
    )]
    ZmViolation {
        /// The feature table.
        table_name: String,
        /// The geometry column.
        column: String,
        /// Which dimension: `"z"` or `"m"`.
        dimension: &'static str,
        /// The column's declared constraint.
        constraint: geopackage_core::types::ZmFlag,
        /// `"carries"` when the geometry has the dimension, `"lacks"` when it
        /// does not.
        verb: &'static str,
    },
}

/// Convenience alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
