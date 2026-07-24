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
}

/// Convenience alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
