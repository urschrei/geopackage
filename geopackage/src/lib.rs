//! Read and write [OGC GeoPackage](https://www.geopackage.org/spec140/) files.
//!
//! A GeoPackage is an SQLite database with a standardised schema for vector
//! features. [`GeoPackage::create`], [`GeoPackage::open`] and
//! [`GeoPackage::open_read_only`] open the container with pragma and schema
//! validation;
//! [`GeoPackage::layer`] returns a [`Layer`] handle for reading and writing
//! features, and [`GeoPackage::layers`] enumerates them.
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
//! // `TableSchemaBuilder::spatial_index(false)` declines it.
//! for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
//!     let feature = feature?;
//!     assert_eq!(feature.value("name"), Some(ValueRef::Text("Dublin")));
//! }
//! # Ok(()) }
//! ```
//!
//! [`Layer::features`] iterates every row as an owned [`Feature`];
//! [`Layer::select`] appends a caller-supplied raw SQL `WHERE` clause;
//! [`Layer::features_in`] runs the bounding-box query, served by the RTree
//! index when one is present and a full scan otherwise, with identical
//! results. Each has a streaming counterpart ([`Layer::cursor`],
//! [`Layer::cursor_select`], [`Layer::cursor_in`]) that reads one row at a
//! time and holds no result set. For writing, [`Layer::write_all`] batch-loads
//! and [`Layer::writer`] opens a transaction with per-row
//! `insert`/`update`/`delete` ([`FeatureWriter`]).
//! [`GeoPackage::create_attributes_table`] and [`GeoPackage::attributes`] are
//! the same for non-spatial attribute tables, and [`GeoPackage::add_epsg_srs`]
//! registers an EPSG code in `gpkg_spatial_ref_sys`.
//! [`GeoPackage::open_lenient`] tolerates legacy and lightly malformed files,
//! collecting [`OpenWarning`]s instead of failing.
//!
//! # Geometry round trips
//!
//! [`Feature::geometry`] parses the stored blob into a
//! [`GpbGeometry`](core::GpbGeometry), a view over the row's bytes that
//! implements [`geo_traits::GeometryTrait`], and every write method accepts
//! any `impl GeometryTrait<T = f64>`. A geometry can therefore be streamed
//! out of one file, measured, and written into another without being
//! converted to a `geo-types` value in either direction if need be: the analysis reads
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
//! # Cargo features
//!
//! - **`geo-types`** (on by default): forwards `geopackage-core`'s feature of
//!   the same name, which adds
//!   [`GpbGeometry::to_geo`](geopackage_core::geometry::GpbGeometry::to_geo).
//!   Decline it with `default-features = false`.
//! - **`arrow`** (off by default): the columnar paths above. It pulls in
//!   `arrow-array` and `arrow-schema`, which a caller using only the scalar API
//!   does not need.
//!
//! # Configuration
//!
//! The defaults are intended to support the common case: a single-file GeoPackage, an indexed
//! feature layer, and values read in keeping with other popular implementations.
//! Each of these types documents possible trade-off behind its defaults:
//!
//! - [`OpenOptions`]: the journal mode ([`JournalMode`], where
//!   [`JournalMode::Wal`] is opt-in) and the `synchronous` durability level
//!   ([`Synchronous`]). Left unset, the file keeps SQLite's own defaults. A
//!   handle that opted into WAL resets the file to a single `DELETE`-journal
//!   file on close, so the `.gpkg` handed on has no sidecar files; see
//!   [`GeoPackage`].
//! - [`TableSchemaBuilder`]: a new layer's columns ([`ColumnSpec`]), primary
//!   key (default [`DEFAULT_PRIMARY_KEY`], `fid`), geometry column
//!   ([`GeometrySpec`], named [`DEFAULT_GEOMETRY_COLUMN`], `geom`, unless told
//!   otherwise), and whether it is indexed
//!   ([`TableSchemaBuilder::spatial_index`], default `true`).
//!   [`Layer::create_spatial_index`], [`Layer::drop_spatial_index`] and
//!   [`Layer::repair_spatial_index`] manage the index after creation.
//! - [`BulkIndexOptions`]: how an RTree index is built, for
//!   [`Layer::create_spatial_index_with`] and [`Layer::write_all_with`]: the
//!   row count at which the bulk build takes over from the per-row triggers
//!   ([`BulkIndexOptions::bulk_threshold`], default [`DEFAULT_BULK_THRESHOLD`],
//!   10,000 rows), the self-check it then runs ([`StructuralCheck`], default
//!   [`StructuralCheck::RtreeOnly`]), and how full each node of the tree is
//!   packed ([`BulkIndexOptions::fill_factor`], default
//!   [`DEFAULT_FILL_FACTOR`], `1.0`).
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
//! hands back the underlying rusqlite connection.
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
// planned `geopackage-ffi` crate (M3) is the sole intended exception, and will
// opt out of the workspace lints rather than relax them here.

#[cfg(feature = "arrow")]
pub mod arrow;
mod bulk;
mod create;
mod error;
mod extent;
mod functions;
mod index;
mod layer;
mod open;
mod options;
mod packed;
mod schema;
mod srs;
mod value;
mod writer;

pub use bulk::{BulkIndexOptions, DEFAULT_BULK_THRESHOLD, DEFAULT_FILL_FACTOR, StructuralCheck};
pub use create::{
    ColumnSpec, DEFAULT_GEOMETRY_COLUMN, DEFAULT_PRIMARY_KEY, GeometrySpec, TableSchemaBuilder,
};
pub use error::{Error, Result};
pub use geopackage_core as core;
pub use geopackage_core::GpkgVersion;
pub use index::SpatialIndexStatus;
pub use layer::{BoundingBox, Feature, FeatureCursor, FeatureStream, Features, Layer, LayerKind};
pub use open::OpenWarning;
pub use options::{JournalMode, OpenOptions, Synchronous};
pub use schema::{Column, GeometryColumn, TableSchema};
pub use srs::Srs;
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
}

impl GeoPackage {
    /// Create a new GeoPackage 1.4 file at `path` with default options
    /// ([`JournalMode::Delete`], SQLite's default `synchronous`).
    ///
    /// Fails if `path` already exists and is non-empty. Use [`OpenOptions`] to
    /// create in [`JournalMode::Wal`] or with an explicit [`Synchronous`] level.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        OpenOptions::new().create(path)
    }

    /// Open an existing GeoPackage read-write with default options.
    ///
    /// Use [`OpenOptions`] to select a journal mode or `synchronous` level.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        OpenOptions::new().open(path)
    }

    /// Open an existing GeoPackage read-only with default options.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> Result<Self> {
        OpenOptions::new().open_read_only(path)
    }

    /// Wrap an already-open connection with default options, validating that it
    /// is a GeoPackage and registering the required SQL functions.
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
            conn: Some(conn),
            version: GpkgVersion::V1_4,
            warnings: Vec::new(),
            journal_mode,
        })
    }

    /// The `open`/`open_read_only` core, applying `options` after opening.
    pub(crate) fn open_configured(
        path: &Path,
        flags: OpenFlags,
        options: OpenOptions,
    ) -> Result<Self> {
        let conn = Connection::open_with_flags(path, flags)?;
        // A read-only connection cannot change its journal mode.
        let apply_journal = flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE);
        Self::from_connection_configured(conn, options, apply_journal)
    }

    /// Validate a connection as a GeoPackage, apply `options`, and register the
    /// SQL functions. `apply_journal` is false for a read-only or
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
        let journal_mode = apply_open_options(&conn, options, apply_journal)?;
        functions::register(&conn)?;
        Ok(Self {
            conn: Some(conn),
            version,
            warnings: Vec::new(),
            journal_mode,
        })
    }

    /// The spec version declared by the file's pragmas.
    pub fn version(&self) -> GpkgVersion {
        self.version
    }

    /// The rows of `gpkg_contents`.
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

    /// Borrow the underlying connection (escape hatch: full SQL access).
    pub fn connection(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("connection is present for the whole handle lifetime")
    }

    /// Consume, returning the underlying connection.
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

    /// Close the GeoPackage, finalising a [`JournalMode::Wal`] file back to a
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

/// Apply `options` to a freshly opened connection, returning the journal mode
/// the resulting handle is responsible for finalising.
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

/// Checkpoint a WAL database fully into the main file and reset it to the
/// `DELETE` rollback journal, removing the `-wal`/`-shm` sidecars.
fn finalize_wal_to_delete(conn: &Connection) -> Result<()> {
    // TRUNCATE flushes every committed frame into the main database and shrinks
    // the WAL to zero; the mode switch then removes the sidecar files.
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))?;
    Ok(())
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
/// exactly. This returns the physical name as SQLite stores it, identical to
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
