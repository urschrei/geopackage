//! Read and write [OGC GeoPackage](https://www.geopackage.org/spec140/) files.
//!
//! **Status: M1 read path.** The container is created and opened with pragma
//! and schema validation ([`GeoPackage::create`], [`GeoPackage::open`],
//! [`GeoPackage::open_read_only`], [`GeoPackage::from_connection`]), and the
//! required RTree SQL functions are registered on every connection. On top of
//! `gpkg_contents`, `gpkg_spatial_ref_sys` ([`Srs`]) and per-table schema
//! introspection ([`TableSchema`]), this crate exposes a read path:
//!
//! - [`GeoPackage::layers`] enumerates feature layers; [`GeoPackage::layer`]
//!   and [`GeoPackage::attributes`] return typed [`Layer`] handles.
//! - [`Layer::features`] iterates a layer as owned [`Feature`]s, with by-name
//!   and by-index value access and lazy geometry parsing.
//! - [`Layer::features_in`] runs a bounding-box query, using the RTree spatial
//!   index when one is present and a full scan otherwise, with provably
//!   identical results.
//! - [`Layer::select`] appends a caller-supplied `WHERE` clause (raw SQL, per
//!   design decision D9: SQL is the query engine).
//! - [`Layer::create_spatial_index`], [`Layer::drop_spatial_index`], and
//!   [`Layer::repair_spatial_index`] manage the RTree spatial index (the
//!   GeoPackage 1.4 trigger set, design decision D7).
//! - [`GeoPackage::open_lenient`] tolerates legacy and lightly malformed files,
//!   collecting [`OpenWarning`]s instead of failing.
//!
//! The bulk-load index path (design decision D8) and the GeoArrow bulk plane
//! arrive in later milestones — see the repository roadmap.
//!
//! ```no_run
//! # fn main() -> Result<(), geopackage::Error> {
//! let gpkg = geopackage::GeoPackage::create("example.gpkg")?;
//! assert!(gpkg.contents()?.is_empty());
//! # Ok(()) }
//! ```

// `unsafe_code = "forbid"` and `missing_docs = "warn"` come from the
// workspace lints table (root Cargo.toml); see roadmap decision D12 for the
// unsafe policy and its single planned exception (`geopackage-ffi`, M3).

mod create;
mod error;
mod functions;
mod index;
mod layer;
mod open;
mod schema;
mod srs;
mod value;
mod writer;

pub use create::{
    ColumnSpec, DEFAULT_GEOMETRY_COLUMN, DEFAULT_PRIMARY_KEY, GeometrySpec, TableSchemaBuilder,
};
pub use error::{Error, Result};
pub use geopackage_core as core;
pub use geopackage_core::GpkgVersion;
pub use layer::{BoundingBox, Feature, Features, Layer, LayerKind};
pub use open::OpenWarning;
pub use schema::{Column, GeometryColumn, TableSchema};
pub use srs::Srs;
pub use value::{ConversionOptions, DateTimeParsing, Value};
pub use writer::{FeatureWriter, NewFeature};

use geopackage_core::{ddl, version};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::Path;

/// An open GeoPackage.
pub struct GeoPackage {
    conn: Connection,
    version: GpkgVersion,
    warnings: Vec<OpenWarning>,
}

impl GeoPackage {
    /// Create a new GeoPackage 1.4 file at `path`.
    ///
    /// Fails if `path` already exists and is non-empty.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if path.exists()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            return Err(Error::AlreadyExists(path.to_owned()));
        }
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
        functions::register(&conn)?;
        Ok(Self {
            conn,
            version: GpkgVersion::V1_4,
            warnings: Vec::new(),
        })
    }

    /// Open an existing GeoPackage read-write.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
    }

    /// Open an existing GeoPackage read-only.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    }

    fn open_with_flags<P: AsRef<Path>>(path: P, flags: OpenFlags) -> Result<Self> {
        let conn = Connection::open_with_flags(path, flags)?;
        Self::from_connection(conn)
    }

    /// Wrap an already-open connection, validating that it is a GeoPackage
    /// and registering the required SQL functions.
    pub fn from_connection(conn: Connection) -> Result<Self> {
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
        functions::register(&conn)?;
        Ok(Self {
            conn,
            version,
            warnings: Vec::new(),
        })
    }

    /// The spec version declared by the file's pragmas.
    pub fn version(&self) -> GpkgVersion {
        self.version
    }

    /// The rows of `gpkg_contents`.
    pub fn contents(&self) -> Result<Vec<ContentsEntry>> {
        let mut stmt = self.conn.prepare(
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

    /// Borrow the underlying connection (escape hatch: full SQL access).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Consume, returning the underlying connection.
    pub fn into_connection(self) -> Connection {
        self.conn
    }
}

/// Read a 32-bit SQLite database-header pragma (`application_id` or
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

pub(crate) fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

/// Resolve `name` to the actual SQLite table (or view) name, matching
/// case-insensitively.
///
/// SQLite object names resolve case-insensitively, but joins between catalogue
/// tables (`gpkg_contents`, `gpkg_geometry_columns`) compare the stored strings
/// exactly. This returns the physical name as SQLite stores it — identical to
/// `name` for a well-formed file, differing only in case for the wrong-case
/// files [`GeoPackage::open_lenient`] tolerates. `None` when no such table
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
