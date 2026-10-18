//! Tile pyramids: creating them and reading existing ones.
//!
//! This module holds [`TilePyramidBuilder`],
//! [`GeoPackage::create_tile_pyramid`], and the [`TilePyramid`] handle over an
//! existing pyramid.
//!
//! A tile pyramid is the container's second data type, alongside features and
//! attributes. Its payloads are opaque here: this crate stores, indexes and
//! validates tiles, but decodes none of them. It does read each payload's
//! header, so it catches a tile written at the wrong pixel size, or in a format
//! the table may not contain, instead of storing it (see
//! [`geopackage_core::tiles::probe`]).
//!
//! The geometry of a pyramid, and the spec's rules about it, live in
//! [`geopackage_core::tiles`]. This module is the part that needs a database:
//! the catalogue rows, the user table, and the extension registrations.
//!
//! Creating a pyramid validates it, but reading one does not. A pyramid written
//! by another implementation opens with whatever its `gpkg_tile_matrix` rows
//! say, because a reader that rejects an imperfect file can't be used to
//! inspect one.

use geopackage_core::ddl;
use geopackage_core::ident::quote;
use geopackage_core::tiles::{
    self, TileCoord, TileFormat, TileMatrix, TileMatrixSet, TilePayload, WEBP_EXTENSION_DEFINITION,
    WEBP_EXTENSION_NAME, ZOOM_OTHER_EXTENSION_DEFINITION, ZOOM_OTHER_EXTENSION_NAME,
};
use rusqlite::types::ValueRef;
use rusqlite::{CachedStatement, Connection, OptionalExtension};

use crate::transaction::WriteTransaction;
use crate::{
    BoundingBox, Error, ExtensionRow, GeoPackage, Result, resolve_table_name, table_exists,
};

/// A handle to one tile pyramid of a [`GeoPackage`].
///
/// You get one from [`GeoPackage::create_tile_pyramid`], [`GeoPackage::tiles`]
/// or [`GeoPackage::tile_pyramids`]. The handle reads the matrix set and the
/// zoom levels once, when it is created, and keeps them sorted by zoom level,
/// so addressing a tile costs a binary search rather than a query. The handle
/// borrows the [`GeoPackage`] for its lifetime.
///
/// Creating a pyramid validates it against the spec's consistency rules, but
/// reading one never does, so a pyramid written by another implementation opens
/// with whatever its `gpkg_tile_matrix` rows say. [`TilePyramid::validate`]
/// checks one on request. Payloads are always opaque: this crate stores and
/// indexes them and checks their headers, but never decodes them.
pub struct TilePyramid<'a> {
    gpkg: &'a GeoPackage,
    table_name: String,
    matrix_set: TileMatrixSet,
    /// Ascending by zoom level, which [`Self::matrix`] binary-searches.
    matrices: Vec<TileMatrix>,
    /// Built once per handle. A statement's text is a `prepare_cached` key, and
    /// formatting one per tile would bring back the per-row rebuild that this
    /// crate has spent releases removing.
    sql: TileSql,
    /// The unidentified extension that blocks writes to this pyramid. Like the
    /// statements, it is read once, because `put_tile` is a per-tile call and a
    /// catalogue query inside it would be paid per tile.
    write_block: Option<ExtensionRow>,
}

impl<'a> TilePyramid<'a> {
    /// Returns the physical SQLite table name backing this pyramid.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Returns the [`GeoPackage`] this pyramid belongs to.
    pub fn gpkg(&self) -> &'a GeoPackage {
        self.gpkg
    }

    /// Returns the pyramid's extent and spatial reference system.
    pub fn matrix_set(&self) -> &TileMatrixSet {
        &self.matrix_set
    }

    /// Returns every declared zoom level, ascending.
    pub fn matrices(&self) -> &[TileMatrix] {
        &self.matrices
    }

    /// Returns the zoom levels this pyramid declares, ascending.
    pub fn zoom_levels(&self) -> Vec<i64> {
        self.matrices.iter().map(|m| m.zoom_level).collect()
    }

    /// Returns the tile matrix for one zoom level, or `None` if the pyramid
    /// does not declare that level.
    pub fn matrix(&self, zoom_level: i64) -> Option<&TileMatrix> {
        self.matrices
            .binary_search_by_key(&zoom_level, |matrix| matrix.zoom_level)
            .ok()
            .and_then(|index| self.matrices.get(index))
    }

    /// Returns one tile's payload, or `None` where the pyramid has no tile at
    /// that address.
    ///
    /// Copies the payload out of SQLite once, which an owned return requires.
    /// [`Self::get_tile_into`] reuses a buffer instead, and [`Self::cursor`]
    /// borrows the bytes without copying at all.
    ///
    /// The address is not checked against the zoom level's grid. A tile outside
    /// the grid is absent, like any other empty address. Writing to an
    /// impossible address is an error.
    pub fn get_tile(&self, coord: TileCoord) -> Result<Option<Vec<u8>>> {
        let conn = self.gpkg.connection();
        let mut stmt = conn.prepare_cached(&self.sql.get)?;
        Ok(stmt
            .query_row(
                rusqlite::params![coord.zoom_level, coord.column, coord.row],
                |row| tile_blob(row, 0).map(<[u8]>::to_vec),
            )
            .optional()?)
    }

    /// Reads one tile's payload into a caller-owned buffer, returning whether
    /// a tile was there.
    ///
    /// The buffer is cleared first and reused, so a loop over many tiles
    /// allocates once rather than once per tile. Its contents are untouched
    /// when the tile is absent.
    pub fn get_tile_into(&self, coord: TileCoord, buffer: &mut Vec<u8>) -> Result<bool> {
        let conn = self.gpkg.connection();
        let mut stmt = conn.prepare_cached(&self.sql.get)?;
        let found = stmt
            .query_row(
                rusqlite::params![coord.zoom_level, coord.column, coord.row],
                |row| {
                    let data = tile_blob(row, 0)?;
                    buffer.clear();
                    buffer.extend_from_slice(data);
                    Ok(())
                },
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Returns `true` if the pyramid has a tile at an address, without
    /// reading its payload.
    pub fn has_tile(&self, coord: TileCoord) -> Result<bool> {
        let conn = self.gpkg.connection();
        let mut stmt = conn.prepare_cached(&self.sql.exists)?;
        Ok(stmt
            .query_row(
                rusqlite::params![coord.zoom_level, coord.column, coord.row],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Returns the number of tiles in the pyramid.
    pub fn tile_count(&self) -> Result<i64> {
        let conn = self.gpkg.connection();
        Ok(conn
            .prepare_cached(&self.sql.count)?
            .query_row([], |r| r.get(0))?)
    }

    /// Returns the number of tiles at one zoom level.
    pub fn tile_count_at(&self, zoom_level: i64) -> Result<i64> {
        let conn = self.gpkg.connection();
        Ok(conn
            .prepare_cached(&self.sql.count_at)?
            .query_row([zoom_level], |r| r.get(0))?)
    }

    /// Streams every tile, in matrix order: by zoom level, then north to
    /// south, then west to east.
    ///
    /// Reading takes two calls, as it does for features. The cursor owns the
    /// statement, and [`TileCursor::tiles`] borrows it to walk the rows. There
    /// is no method that collects the tiles, because one zoom level of a real
    /// pyramid is more payload than a `Vec` of them should contain.
    pub fn cursor(&self) -> Result<TileCursor<'_>> {
        self.cursor_with(&self.sql.scan, Vec::new())
    }

    /// Streams the tiles of one zoom level, in matrix order.
    pub fn cursor_at(&self, zoom_level: i64) -> Result<TileCursor<'_>> {
        self.cursor_with(&self.sql.scan_at, vec![zoom_level.into()])
    }

    /// Streams the tiles of one zoom level that a bounding box touches, in
    /// matrix order.
    ///
    /// The box is in the pyramid's own spatial reference system. This crate
    /// transforms nothing. The box becomes a range of tile indices and a single
    /// query on the table, so only the payloads the box selects are read. A box
    /// that misses the pyramid's extent yields no tiles.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownZoomLevel`] when the pyramid declares no such zoom
    /// level, since without its grid a box cannot be turned into tile indices.
    pub fn cursor_in(&self, zoom_level: i64, bbox: BoundingBox) -> Result<TileCursor<'_>> {
        let matrix = self
            .matrix(zoom_level)
            .ok_or_else(|| Error::UnknownZoomLevel {
                table_name: self.table_name.clone(),
                zoom_level,
            })?;
        // A box outside the extent binds an empty range rather than taking a
        // path of its own: the query then returns nothing, which is the answer.
        let range = self
            .matrix_set
            .tile_range(matrix, bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y);
        let (min_column, max_column, min_row, max_row) = range.map_or((0, -1, 0, -1), |range| {
            (
                range.min_column,
                range.max_column,
                range.min_row,
                range.max_row,
            )
        });
        self.cursor_with(
            &self.sql.scan_in,
            vec![
                zoom_level.into(),
                min_column.into(),
                max_column.into(),
                min_row.into(),
                max_row.into(),
            ],
        )
    }

    fn cursor_with(
        &self,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
    ) -> Result<TileCursor<'_>> {
        let stmt = self.gpkg.connection().prepare(sql)?;
        Ok(TileCursor { stmt, params })
    }

    /// Writes one tile, replacing whatever was at that address.
    ///
    /// The tile is validated. The zoom level must be one the pyramid declares,
    /// the column and row must fall inside that level's grid, and the payload
    /// must be a PNG or JPEG (or a WebP, which also registers `gpkg_webp`) of
    /// exactly the pixel size the zoom level declares. No conforming reader
    /// could use a tile that fails any of these checks, so it is rejected
    /// rather than stored.
    ///
    /// One tile per transaction. Use [`Self::writer`] or [`Self::write_all`] to
    /// write many.
    ///
    /// # Errors
    ///
    /// - [`Error::UnknownZoomLevel`] for a zoom level with no
    ///   `gpkg_tile_matrix` row.
    /// - [`Error::Tile`] for an address outside the grid, an unreadable
    ///   payload, or one of the wrong pixel size.
    /// - [`Error::TileFormatNotAllowed`] for a payload that is not PNG, JPEG or
    ///   WebP.
    pub fn put_tile(&self, coord: TileCoord, data: &[u8]) -> Result<()> {
        let mut writer = self.writer()?;
        writer.put(coord, data)?;
        writer.commit()
    }

    /// Deletes one tile, returning whether there was one to delete.
    pub fn delete_tile(&self, coord: TileCoord) -> Result<bool> {
        let mut writer = self.writer()?;
        let deleted = writer.delete(coord)?;
        writer.commit()?;
        Ok(deleted)
    }

    /// Opens a [`TileWriter`]: one transaction, prepared statements, and
    /// per-tile `put`/`delete`.
    ///
    /// Nothing is written until [`TileWriter::commit`]. Dropping the writer
    /// rolls its transaction back.
    pub fn writer(&self) -> Result<TileWriter<'a>> {
        TileWriter::new(self)
    }

    /// Returns the extension this crate cannot identify that stops it writing
    /// to this pyramid, if there is one.
    ///
    /// This is read when the handle is opened, so it does not reflect rows
    /// registered since then. See [`GeoPackage::blocking_extension`].
    pub fn blocking_extension(&self) -> Option<&ExtensionRow> {
        self.write_block.as_ref()
    }

    /// Rejects a write when an unidentified extension covers the pyramid.
    fn check_writable(&self) -> Result<()> {
        match &self.write_block {
            None => Ok(()),
            Some(row) => Err(Error::UnsupportedExtension {
                table_name: self.table_name.clone(),
                extension_name: row.name.clone(),
                scope: row.scope.as_str().to_owned(),
            }),
        }
    }

    /// Writes many tiles, committing every `batch_size` of them (`0` writes
    /// them all in one transaction), and returns how many were written.
    ///
    /// Payloads are borrowed, not consumed. Anything that implements
    /// `AsRef<[u8]>` works, so tiles copied from another pyramid go straight
    /// from one row's buffer into the other's statement without a copy in
    /// between.
    ///
    /// # Errors
    ///
    /// As [`Self::put_tile`], at the first tile that fails. Tiles committed in
    /// earlier batches stay written. The batch in flight is rolled back.
    pub fn write_all<D: AsRef<[u8]>>(
        &self,
        tiles: impl IntoIterator<Item = (TileCoord, D)>,
        batch_size: usize,
    ) -> Result<usize> {
        let mut tiles = tiles.into_iter();
        let mut total = 0;
        loop {
            let mut writer = self.writer()?;
            let mut in_batch = 0;
            for (coord, data) in tiles.by_ref() {
                writer.put(coord, data.as_ref())?;
                in_batch += 1;
                if batch_size != 0 && in_batch == batch_size {
                    break;
                }
            }
            writer.commit()?;
            total += in_batch;
            if batch_size == 0 || in_batch < batch_size {
                return Ok(total);
            }
        }
    }

    /// Returns `Ok` if this pyramid satisfies the spec's consistency rules.
    ///
    /// Creation checks this, so a pyramid this crate wrote always passes. Use
    /// it on a pyramid from a file written elsewhere, because every tile bound
    /// is calculated from values that are not otherwise checked.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] naming the first rule the pyramid breaks.
    pub fn validate(&self) -> Result<()> {
        self.matrix_set.validate(&self.matrices)?;
        Ok(())
    }
}

impl std::fmt::Debug for TilePyramid<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TilePyramid")
            .field("table_name", &self.table_name)
            .field("matrix_set", &self.matrix_set)
            .field("zoom_levels", &self.zoom_levels())
            .finish()
    }
}

/// The statement text a [`TilePyramid`] uses, built once at construction.
#[derive(Debug, Clone)]
struct TileSql {
    get: String,
    exists: String,
    count: String,
    count_at: String,
    scan: String,
    scan_at: String,
    scan_in: String,
    put: String,
    delete: String,
}

impl TileSql {
    /// Builds the statements for a table, whose name is quoted once here
    /// rather than at every call.
    fn new(table: &str) -> Result<Self> {
        let table = quote(table)?;
        // Matrix order, which is what a consumer walking a pyramid expects:
        // zoom level, then north to south, then west to east.
        let order = "ORDER BY zoom_level, tile_row, tile_column";
        let columns = "zoom_level, tile_column, tile_row, tile_data";
        let address = "zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3";
        Ok(Self {
            get: format!("SELECT tile_data FROM {table} WHERE {address}"),
            exists: format!("SELECT 1 FROM {table} WHERE {address}"),
            count: format!("SELECT count(*) FROM {table}"),
            count_at: format!("SELECT count(*) FROM {table} WHERE zoom_level = ?1"),
            scan: format!("SELECT {columns} FROM {table} {order}"),
            scan_at: format!("SELECT {columns} FROM {table} WHERE zoom_level = ?1 {order}"),
            scan_in: format!(
                "SELECT {columns} FROM {table} \
                 WHERE zoom_level = ?1 AND tile_column BETWEEN ?2 AND ?3 \
                 AND tile_row BETWEEN ?4 AND ?5 {order}"
            ),
            put: format!(
                "INSERT INTO {table} (zoom_level, tile_column, tile_row, tile_data) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (zoom_level, tile_column, tile_row) \
                 DO UPDATE SET tile_data = excluded.tile_data"
            ),
            delete: format!("DELETE FROM {table} WHERE {address}"),
        })
    }
}

/// The input for creating a tile table, for either kind of content.
///
/// A tile pyramid and a tiled gridded coverage have the same physical
/// structure: the user table, the matrix set, the zoom levels and the
/// `gpkg_contents` row. They differ in the `data_type` of that row, and in the
/// other tables that they use. This struct is the shared part, so
/// [`crate::Coverage`] does not duplicate it.
pub(crate) struct PyramidSpec<'a> {
    pub(crate) table_name: &'a str,
    pub(crate) matrix_set: &'a TileMatrixSet,
    pub(crate) matrices: &'a [TileMatrix],
    pub(crate) identifier: Option<&'a str>,
    pub(crate) description: Option<&'a str>,
    /// `tiles`, or `2d-gridded-coverage` (Requirement 5).
    pub(crate) data_type: &'static str,
    pub(crate) allow_zoom_other: bool,
}

impl GeoPackage {
    /// Creates a tile pyramid from a [`TilePyramidBuilder`].
    ///
    /// Writes the tile pyramid user table, a `gpkg_contents` row
    /// (`data_type = 'tiles'`, bounded by the matrix set extent), the
    /// `gpkg_tile_matrix_set` row and one `gpkg_tile_matrix` row per zoom
    /// level, in one transaction, then returns a handle to the new pyramid.
    /// `gpkg_tile_matrix_set` and `gpkg_tile_matrix` are created on first use,
    /// as `gpkg_geometry_columns` is for feature layers.
    ///
    /// # Errors
    ///
    /// - [`Error::Core`] with
    ///   [`InvalidIdentifier`](geopackage_core::Error::InvalidIdentifier) if
    ///   the name is empty or contains NUL.
    /// - [`Error::ReservedTablePrefix`] if the name begins `gpkg_` or
    ///   `sqlite_`, in any case.
    /// - [`Error::TableAlreadyExists`] if a table or view of that name exists.
    /// - [`Error::UnknownSrs`] if the matrix set's `srs_id` is not registered
    ///   in `gpkg_spatial_ref_sys`.
    /// - [`Error::Tile`] if the pyramid breaks one of the spec's consistency
    ///   rules (Requirements 45 to 53).
    /// - [`Error::ZoomOtherNotEnabled`] if its zoom levels do not step by
    ///   factors of two and [`TilePyramidBuilder::allow_zoom_other`] was not
    ///   set.
    pub fn create_tile_pyramid(&self, builder: &TilePyramidBuilder) -> Result<TilePyramid<'_>> {
        let spec = PyramidSpec {
            table_name: &builder.table_name,
            matrix_set: &builder.matrix_set,
            matrices: &builder.matrices,
            identifier: builder.identifier.as_deref(),
            description: builder.description.as_deref(),
            data_type: "tiles",
            allow_zoom_other: builder.allow_zoom_other,
        };
        let zoom_other = self.check_new_pyramid(&spec)?;
        let tx = WriteTransaction::begin(self.connection())?;
        self.write_pyramid(&spec, zoom_other)?;
        tx.commit()?;
        self.tiles(&builder.table_name)
    }

    /// Runs every check that is possible before a write. Returns `true` if the
    /// pyramid needs `gpkg_zoom_other`.
    ///
    /// This is separate from [`Self::write_pyramid`] so that a failed check
    /// happens outside the transaction and does not cause a rollback.
    pub(crate) fn check_new_pyramid(&self, spec: &PyramidSpec<'_>) -> Result<bool> {
        let name = spec.table_name;
        // As with `create_layer`: a whole-GeoPackage extension we cannot
        // identify covers a table that does not exist yet.
        self.check_writable(name)?;
        crate::create::check_new_table_name(name)?;
        let conn = self.connection();
        if table_exists(conn, name)? {
            return Err(Error::TableAlreadyExists {
                table_name: name.to_owned(),
            });
        }
        if self.srs(spec.matrix_set.srs_id)?.is_none() {
            return Err(Error::UnknownSrs {
                srs_id: spec.matrix_set.srs_id,
            });
        }
        spec.matrix_set.validate(spec.matrices)?;
        let zoom_other = !tiles::is_power_of_two_ladder(spec.matrices);
        if zoom_other && !spec.allow_zoom_other {
            return Err(Error::ZoomOtherNotEnabled {
                table_name: name.to_owned(),
            });
        }
        Ok(zoom_other)
    }

    /// Writes the user table and the catalogue rows, in a transaction that the
    /// caller owns.
    pub(crate) fn write_pyramid(&self, spec: &PyramidSpec<'_>, zoom_other: bool) -> Result<()> {
        let conn = self.connection();
        let name = spec.table_name;
        let set = spec.matrix_set;
        let identifier = spec.identifier.unwrap_or(name);
        let description = spec.description.unwrap_or("");

        for (exists, sql) in [
            (
                table_exists(conn, "gpkg_tile_matrix_set")?,
                ddl::CREATE_GPKG_TILE_MATRIX_SET,
            ),
            (
                table_exists(conn, "gpkg_tile_matrix")?,
                ddl::CREATE_GPKG_TILE_MATRIX,
            ),
        ] {
            if !exists {
                conn.execute_batch(sql)?;
            }
        }
        conn.execute_batch(&tiles::create_tile_table_sql(name)?)?;
        // The extent is the matrix set's, not a measurement: for tiles it is
        // exact by Requirement 144, and gpkg_contents records the same box.
        conn.execute(
            "INSERT INTO gpkg_contents \
             (table_name, data_type, identifier, description, min_x, min_y, max_x, max_y, srs_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                name,
                spec.data_type,
                identifier,
                description,
                set.min_x,
                set.min_y,
                set.max_x,
                set.max_y,
                set.srs_id,
            ],
        )?;
        conn.execute(
            "INSERT INTO gpkg_tile_matrix_set (table_name, srs_id, min_x, min_y, max_x, max_y) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![name, set.srs_id, set.min_x, set.min_y, set.max_x, set.max_y],
        )?;
        {
            let mut stmt = conn.prepare(
                "INSERT INTO gpkg_tile_matrix \
                 (table_name, zoom_level, matrix_width, matrix_height, tile_width, tile_height, \
                  pixel_x_size, pixel_y_size) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for matrix in spec.matrices {
                stmt.execute(rusqlite::params![
                    name,
                    matrix.zoom_level,
                    matrix.matrix_width,
                    matrix.matrix_height,
                    matrix.tile_width,
                    matrix.tile_height,
                    matrix.pixel_x_size,
                    matrix.pixel_y_size,
                ])?;
            }
        }
        if zoom_other {
            crate::extensions::register(
                conn,
                Some(name),
                Some(tiles::TILE_DATA_COLUMN),
                ZOOM_OTHER_EXTENSION_NAME,
                ZOOM_OTHER_EXTENSION_DEFINITION,
                tiles::TILE_EXTENSION_SCOPE,
            )?;
        }
        Ok(())
    }

    /// Opens a tile pyramid by name.
    ///
    /// Nothing is validated. The matrix set and zoom levels are reported as the
    /// file records them.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchLayer`] if `name` is not in `gpkg_contents`.
    /// - [`Error::WrongDataType`] if it is registered but not as `tiles`.
    /// - [`Error::NoTileMatrixSet`] if its `gpkg_tile_matrix_set` row is
    ///   missing, which leaves its tiles unlocatable.
    pub fn tiles(&self, name: &str) -> Result<TilePyramid<'_>> {
        self.open_pyramid(name, "tiles")
    }

    /// Opens a pyramid for a `gpkg_contents` row of the given data type.
    ///
    /// The tile table, its matrix set and its zoom levels have the same
    /// structure for images and for measurements, so a coverage
    /// (`2d-gridded-coverage`, Requirement 5) uses all of them. A coverage
    /// does not expose the pyramid handle publicly, though.
    /// [`crate::Coverage`] keeps its [`TilePyramid`] private, so a coverage
    /// never offers a writer that does not know about its ancillary rows.
    pub(crate) fn open_pyramid(
        &self,
        name: &str,
        expected_data_type: &'static str,
    ) -> Result<TilePyramid<'_>> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT table_name, data_type FROM gpkg_contents \
                 WHERE table_name = ?1 COLLATE NOCASE",
                [name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let (declared_name, data_type) = row.ok_or_else(|| Error::NoSuchLayer {
            table_name: name.to_owned(),
        })?;
        if data_type != expected_data_type {
            return Err(Error::WrongDataType {
                table_name: declared_name,
                expected: expected_data_type,
                found: data_type,
            });
        }
        let table_name = resolve_table_name(conn, &declared_name)?.unwrap_or(declared_name);
        let matrix_set =
            read_matrix_set(conn, &table_name)?.ok_or_else(|| Error::NoTileMatrixSet {
                table_name: table_name.clone(),
            })?;
        let matrices = read_matrices(conn, &table_name)?;
        let sql = TileSql::new(&table_name)?;
        let write_block = self.blocking_extension(&table_name)?;
        Ok(TilePyramid {
            gpkg: self,
            table_name,
            matrix_set,
            matrices,
            sql,
            write_block,
        })
    }

    /// Returns every tile pyramid in the file, by `gpkg_contents` name.
    pub fn tile_pyramids(&self) -> Result<Vec<TilePyramid<'_>>> {
        self.tile_pyramid_names()?
            .iter()
            .map(|name| self.tiles(name))
            .collect()
    }

    /// Returns the `gpkg_contents` name of every tile pyramid, without
    /// opening the pyramids.
    pub(crate) fn tile_pyramid_names(&self) -> Result<Vec<String>> {
        if !table_exists(self.connection(), "gpkg_tile_matrix_set")? {
            return Ok(Vec::new());
        }
        let mut stmt = self.connection().prepare(
            "SELECT c.table_name FROM gpkg_contents c \
             JOIN gpkg_tile_matrix_set s ON s.table_name = c.table_name COLLATE NOCASE \
             WHERE c.data_type = 'tiles' ORDER BY c.table_name",
        )?;
        let names = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(names)
    }
}

/// A declarative builder for a tile pyramid.
///
/// You declare the pyramid's extent and spatial reference system
/// ([`TileMatrixSet`]), its zoom levels ([`TileMatrix`]) and the catalogue
/// metadata, then pass the builder to [`GeoPackage::create_tile_pyramid`].
///
/// You usually derive the zoom levels from the extent rather than writing them
/// out. [`TileMatrixSet::ladder`] builds a power-of-two ladder whose pixel
/// sizes span the extent exactly.
///
/// ```
/// use geopackage::core::tiles::{TileMatrixSet, ZoomLadder};
/// use geopackage::{GeoPackage, TilePyramidBuilder};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("basemap.gpkg");
/// let gpkg = GeoPackage::create(path)?;
/// gpkg.add_epsg_srs(3857)?;
///
/// let matrix_set = TileMatrixSet::web_mercator_quad();
/// let matrices = matrix_set.ladder(ZoomLadder::new(0, 4))?;
/// let tiles = gpkg.create_tile_pyramid(
///     &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices),
/// )?;
///
/// assert_eq!(tiles.zoom_levels(), vec![0, 1, 2, 3, 4]);
/// assert_eq!(tiles.matrix(4).map(|m| m.matrix_width), Some(16));
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct TilePyramidBuilder {
    table_name: String,
    identifier: Option<String>,
    description: Option<String>,
    matrix_set: TileMatrixSet,
    matrices: Vec<TileMatrix>,
    allow_zoom_other: bool,
}

impl TilePyramidBuilder {
    /// Starts a builder for a pyramid of the given name over the given
    /// extent.
    ///
    /// The name is validated when the builder reaches
    /// [`GeoPackage::create_tile_pyramid`], not here.
    pub fn new(table_name: impl Into<String>, matrix_set: TileMatrixSet) -> Self {
        Self {
            table_name: table_name.into(),
            identifier: None,
            description: None,
            matrix_set,
            matrices: Vec::new(),
            allow_zoom_other: false,
        }
    }

    /// Declares one zoom level.
    #[must_use]
    pub fn matrix(mut self, matrix: TileMatrix) -> Self {
        self.matrices.push(matrix);
        self
    }

    /// Declares a set of zoom levels, in any order.
    #[must_use]
    pub fn matrices(mut self, matrices: impl IntoIterator<Item = TileMatrix>) -> Self {
        self.matrices.extend(matrices);
        self
    }

    /// Sets `gpkg_contents.identifier` (a human-readable name). Defaults to
    /// the table name when left unset.
    #[must_use]
    pub fn identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Sets `gpkg_contents.description`.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Allows zoom levels that do not step by factors of two, registering the
    /// `gpkg_zoom_other` extension for the table.
    ///
    /// Off by default. Without it, such zoom levels are an error rather than a
    /// silent registration, because a ladder that does not double is usually a
    /// mistake in the pixel sizes, and readers of a file that quietly registers
    /// an extension may not support it. Such a pyramid is always *read* whether
    /// or not this was set, as it is for the file's original writer.
    #[must_use]
    pub fn allow_zoom_other(mut self, allow: bool) -> Self {
        self.allow_zoom_other = allow;
        self
    }

    /// Returns the table name.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

/// A transaction over one pyramid, with per-tile `put` and `delete`.
///
/// You get one from [`TilePyramid::writer`]. Writes are staged in the
/// transaction the writer owns, and [`Self::commit`] refreshes
/// `gpkg_contents.last_change` and commits. Dropping a writer without
/// committing rolls it back.
///
/// The exception is when a transaction was already open on the connection. The
/// writer then joins it, and committing or rolling back is left to whoever
/// began it. See [`Self::commit`], and [`crate::FeatureWriter::commit`] for the
/// full reasoning.
///
/// The statements are prepared on the connection rather than on the
/// transaction, so they borrow what the transaction borrows instead of
/// borrowing the transaction itself, as [`crate::FeatureWriter`]'s statements
/// do. They still run inside the transaction, because a SQLite transaction
/// belongs to the connection, not to the statements prepared against it.
pub struct TileWriter<'conn> {
    tx: WriteTransaction<'conn>,
    conn: &'conn Connection,
    table_name: String,
    /// The pyramid's zoom levels, ascending. They are copied rather than
    /// borrowed so the writer has one lifetime. A pyramid has tens of levels,
    /// so this is one allocation per writer, against a binary search per tile.
    matrices: Vec<TileMatrix>,
    /// `INSERT ... ON CONFLICT DO UPDATE`. Writing a tile over an existing one
    /// replaces the payload and keeps the row's id, which the metadata
    /// extension may reference.
    put_stmt: CachedStatement<'conn>,
    delete_stmt: CachedStatement<'conn>,
    /// Whether anything has been written, so an untouched writer does not stamp
    /// `last_change`.
    dirty: bool,
    /// Whether `gpkg_webp` is registered for this table. Read once, on the
    /// first WebP payload, so a pyramid of PNGs never asks.
    webp_registered: Option<bool>,
}

impl<'conn> TileWriter<'conn> {
    fn new(pyramid: &TilePyramid<'conn>) -> Result<Self> {
        // Every tile write reaches a writer, `put_tile` and `delete_tile`
        // included, so this is the one place the check belongs.
        pyramid.check_writable()?;
        let conn = pyramid.gpkg.connection();
        let tx = WriteTransaction::begin(conn)?;
        let put_stmt = conn.prepare_cached(&pyramid.sql.put)?;
        let delete_stmt = conn.prepare_cached(&pyramid.sql.delete)?;
        Ok(Self {
            tx,
            conn,
            table_name: pyramid.table_name.clone(),
            matrices: pyramid.matrices.clone(),
            put_stmt,
            delete_stmt,
            dirty: false,
            webp_registered: None,
        })
    }

    /// Writes one tile, replacing whatever was at that address.
    ///
    /// # Errors
    ///
    /// As [`TilePyramid::put_tile`], which is this call in its own
    /// transaction.
    pub fn put(&mut self, coord: TileCoord, data: &[u8]) -> Result<()> {
        let matrix = self
            .matrices
            .binary_search_by_key(&coord.zoom_level, |matrix| matrix.zoom_level)
            .ok()
            .and_then(|index| self.matrices.get(index))
            .ok_or_else(|| Error::UnknownZoomLevel {
                table_name: self.table_name.clone(),
                zoom_level: coord.zoom_level,
            })?;
        matrix.check_contains(coord.column, coord.row)?;
        let payload = tiles::probe(data)?;
        matrix.check_payload(&payload)?;
        match payload.format {
            TileFormat::Webp => self.register_webp()?,
            format if format.is_core() => {}
            format => {
                return Err(Error::TileFormatNotAllowed {
                    table_name: self.table_name.clone(),
                    format,
                });
            }
        }
        // The payload binds as a borrowed slice: a tile read from one pyramid
        // reaches another's statement without being copied on the way.
        self.put_stmt.execute(rusqlite::params![
            coord.zoom_level,
            coord.column,
            coord.row,
            data
        ])?;
        self.dirty = true;
        Ok(())
    }

    /// Deletes one tile, returning whether there was one to delete.
    pub fn delete(&mut self, coord: TileCoord) -> Result<bool> {
        let deleted = self.delete_stmt.execute(rusqlite::params![
            coord.zoom_level,
            coord.column,
            coord.row
        ])?;
        self.dirty |= deleted > 0;
        Ok(deleted > 0)
    }

    /// Refreshes `gpkg_contents.last_change` and commits.
    ///
    /// A writer that wrote nothing commits an empty transaction and leaves
    /// `last_change` alone.
    ///
    /// # When the transaction was the caller's
    ///
    /// As with [`crate::FeatureWriter::commit`]. A writer opened while a
    /// transaction was already open has joined it, so this stages the
    /// `last_change` refresh and returns success without committing. Dropping
    /// such a writer rolls nothing back.
    pub fn commit(self) -> Result<()> {
        let Self {
            tx,
            conn,
            table_name,
            dirty,
            put_stmt,
            delete_stmt,
            ..
        } = self;
        // Statements borrow the connection, not the transaction, but dropping
        // them here keeps the cache tidy before the commit.
        drop(put_stmt);
        drop(delete_stmt);
        if dirty {
            conn.execute(
                "UPDATE gpkg_contents \
                 SET last_change = strftime('%Y-%m-%dT%H:%M:%fZ','now') \
                 WHERE table_name = ?1",
                [&table_name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Registers `gpkg_webp` for this table, once, on the first WebP
    /// payload.
    fn register_webp(&mut self) -> Result<()> {
        if self.webp_registered.is_none() {
            self.webp_registered = Some(crate::extensions::is_registered(
                self.conn,
                Some(&self.table_name),
                WEBP_EXTENSION_NAME,
            )?);
        }
        if self.webp_registered == Some(false) {
            crate::extensions::register(
                self.conn,
                Some(&self.table_name),
                Some(tiles::TILE_DATA_COLUMN),
                WEBP_EXTENSION_NAME,
                WEBP_EXTENSION_DEFINITION,
                tiles::TILE_EXTENSION_SCOPE,
            )?;
            self.webp_registered = Some(true);
        }
        Ok(())
    }
}

impl std::fmt::Debug for TileWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileWriter")
            .field("table_name", &self.table_name)
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

/// A prepared tile scan, owning its statement so that [`TileCursor::tiles`]
/// can borrow each payload straight out of the row.
///
/// [`crate::FeatureCursor`] is split the same way, for the same reason.
/// rusqlite's row cursor borrows its statement, so an iterator owning both
/// would be self-referential, which `#![forbid(unsafe_code)]` rules out.
pub struct TileCursor<'a> {
    stmt: rusqlite::Statement<'a>,
    params: Vec<rusqlite::types::Value>,
}

impl TileCursor<'_> {
    /// Runs the scan and walks its tiles.
    ///
    /// Each call re-runs the query from the start, so a cursor can be walked
    /// more than once.
    pub fn tiles(&mut self) -> Result<TileStream<'_>> {
        let rows = self
            .stmt
            .query(rusqlite::params_from_iter(self.params.iter()))?;
        Ok(TileStream { rows })
    }
}

impl std::fmt::Debug for TileCursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileCursor")
            .field("parameters", &self.params.len())
            .finish_non_exhaustive()
    }
}

/// A scan in progress, borrowing one tile at a time.
///
/// This is not an [`Iterator`], because an iterator's item cannot borrow from
/// the iterator, and this type exists to return the payload without copying it.
/// Walk it with `while let Some(tile) = stream.next()?`, or pass a closure to
/// [`Self::for_each`].
///
/// ```
/// # use geopackage::core::tiles::{TileMatrixSet, ZoomLadder};
/// # use geopackage::{GeoPackage, TilePyramidBuilder};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let dir = tempfile::tempdir()?;
/// # let gpkg = GeoPackage::create(dir.path().join("t.gpkg"))?;
/// # gpkg.add_epsg_srs(3857)?;
/// # let set = TileMatrixSet::web_mercator_quad();
/// # let matrices = set.ladder(ZoomLadder::new(0, 2))?;
/// # let pyramid = gpkg.create_tile_pyramid(&TilePyramidBuilder::new("basemap", set).matrices(matrices))?;
/// let mut cursor = pyramid.cursor()?;
/// let mut stream = cursor.tiles()?;
/// let mut bytes = 0;
/// while let Some(tile) = stream.next()? {
///     // `tile.data()` borrows the row: nothing is copied to count it.
///     bytes += tile.data().len();
/// }
/// # assert_eq!(bytes, 0);
/// # Ok(()) }
/// ```
pub struct TileStream<'c> {
    rows: rusqlite::Rows<'c>,
}

impl TileStream<'_> {
    /// Returns the next tile of the scan, or `None` at its end.
    ///
    /// The returned [`Tile`] borrows this stream, so it is dropped before the
    /// next call. Copy what you need out of it with [`Tile::to_vec`].
    #[expect(
        clippy::should_implement_trait,
        reason = "a lending cursor cannot implement Iterator: its item borrows the iterator. `next` is the name a caller expects in a `while let` loop, and the fallible, borrowing signature is visibly not Iterator::next"
    )]
    pub fn next(&mut self) -> Result<Option<Tile<'_>>> {
        let Some(row) = self.rows.next()? else {
            return Ok(None);
        };
        Ok(Some(Tile {
            coord: TileCoord::new(row.get(0)?, row.get(1)?, row.get(2)?),
            data: tile_blob(row, 3)?,
        }))
    }

    /// Runs a closure over every remaining tile.
    ///
    /// Walks the tiles like [`Self::next`], but handles the borrow for you. An
    /// error from the closure ends the scan.
    ///
    /// # Errors
    ///
    /// Whatever the closure returns, or a read error from the scan itself.
    pub fn for_each(&mut self, mut f: impl FnMut(&Tile<'_>) -> Result<()>) -> Result<()> {
        while let Some(tile) = self.next()? {
            f(&tile)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for TileStream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `rusqlite::Rows` is not `Debug`, and a scan mid-flight has no state
        // worth printing beyond the query that produced it.
        f.debug_struct("TileStream").finish_non_exhaustive()
    }
}

/// One tile of a pyramid, borrowing its payload from the row it was read from.
///
/// The payload is the bytes as stored, in whatever format the table contains.
/// This crate does not decode them. [`Tile::to_vec`] copies, and everything
/// else here borrows.
#[derive(Debug)]
pub struct Tile<'a> {
    coord: TileCoord,
    data: &'a [u8],
}

impl Tile<'_> {
    /// Returns where this tile sits in the pyramid.
    pub fn coord(&self) -> TileCoord {
        self.coord
    }

    /// Returns the stored payload, borrowed from SQLite's row buffer.
    pub fn data(&self) -> &[u8] {
        self.data
    }

    /// Returns the stored payload, copied into an owned buffer.
    pub fn to_vec(&self) -> Vec<u8> {
        self.data.to_vec()
    }

    /// Returns what the payload's header declares: its encoding and pixel
    /// size.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] when the bytes are not a readable image header.
    pub fn probe(&self) -> Result<TilePayload> {
        Ok(tiles::probe(self.data)?)
    }
}

/// A tile payload, borrowed from the row rather than copied out of it.
///
/// `tile_data` is `NOT NULL` in every table this crate creates, and a
/// non-blob there is a malformed file rather than a value to coerce.
fn tile_blob<'a>(row: &'a rusqlite::Row<'a>, index: usize) -> rusqlite::Result<&'a [u8]> {
    match row.get_ref(index)? {
        ValueRef::Blob(data) => Ok(data),
        _ => Err(rusqlite::Error::InvalidColumnType(
            index,
            tiles::TILE_DATA_COLUMN.to_owned(),
            rusqlite::types::Type::Blob,
        )),
    }
}

/// Reads a pyramid's `gpkg_tile_matrix_set` row.
fn read_matrix_set(conn: &Connection, table: &str) -> Result<Option<TileMatrixSet>> {
    Ok(conn
        .query_row(
            "SELECT srs_id, min_x, min_y, max_x, max_y FROM gpkg_tile_matrix_set \
             WHERE table_name = ?1 COLLATE NOCASE",
            [table],
            |r| {
                Ok(TileMatrixSet {
                    srs_id: r.get(0)?,
                    min_x: r.get(1)?,
                    min_y: r.get(2)?,
                    max_x: r.get(3)?,
                    max_y: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Reads a pyramid's `gpkg_tile_matrix` rows, ascending by zoom level.
///
/// A file with no `gpkg_tile_matrix` table at all has no zoom levels. That is
/// an empty pyramid, not an error, because the tiles table may be empty and the
/// spec requires a row only for a level that contains tiles.
fn read_matrices(conn: &Connection, table: &str) -> Result<Vec<TileMatrix>> {
    if !table_exists(conn, "gpkg_tile_matrix")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT zoom_level, matrix_width, matrix_height, tile_width, tile_height, \
         pixel_x_size, pixel_y_size FROM gpkg_tile_matrix \
         WHERE table_name = ?1 COLLATE NOCASE ORDER BY zoom_level",
    )?;
    let rows = stmt.query_map([table], |r| {
        Ok(TileMatrix {
            zoom_level: r.get(0)?,
            matrix_width: r.get(1)?,
            matrix_height: r.get(2)?,
            tile_width: r.get(3)?,
            tile_height: r.get(4)?,
            pixel_x_size: r.get(5)?,
            pixel_y_size: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
