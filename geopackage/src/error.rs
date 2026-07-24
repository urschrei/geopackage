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
}

/// Convenience alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
