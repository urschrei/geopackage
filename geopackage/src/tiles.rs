//! Tile pyramids: [`TilePyramidBuilder`], [`GeoPackage::create_tile_pyramid`],
//! and the [`TilePyramid`] handle over an existing one.
//!
//! A tile pyramid is the container's second data type, alongside features and
//! attributes, and its payloads are opaque here: this crate stores, indexes and
//! validates tiles, and decodes none of them. What it does read is each
//! payload's header, which is how a tile written at the wrong pixel size, or in
//! a format the table may not hold, is caught rather than stored (see
//! [`geopackage_core::tiles::probe`]).
//!
//! The geometry of a pyramid, and the spec's rules about it, live in
//! [`geopackage_core::tiles`]. This module is the part that needs a database:
//! the catalogue rows, the user table, and the extension registrations.
//!
//! Creation validates; reading does not. A pyramid another implementation wrote
//! opens on whatever its `gpkg_tile_matrix` rows say, because a reader that
//! refuses an imperfect file is of no use for looking at one.

use geopackage_core::ddl;
use geopackage_core::tiles::{
    self, TileMatrix, TileMatrixSet, ZOOM_OTHER_EXTENSION_DEFINITION, ZOOM_OTHER_EXTENSION_NAME,
};
use rusqlite::{Connection, OptionalExtension};

use crate::{Error, GeoPackage, Result, resolve_table_name, table_exists};

/// A declarative builder for a tile pyramid.
///
/// Carries the pyramid's extent and spatial reference system
/// ([`TileMatrixSet`]), the zoom levels it declares ([`TileMatrix`]), and the
/// catalogue metadata, then goes to [`GeoPackage::create_tile_pyramid`].
///
/// The zoom levels are usually built from the extent rather than written out:
/// [`TileMatrixSet::ladder`] derives a power-of-two ladder whose pixel sizes
/// span the extent exactly.
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
    /// Start a builder for a pyramid of the given name over the given extent.
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

    /// Declare one zoom level.
    #[must_use]
    pub fn matrix(mut self, matrix: TileMatrix) -> Self {
        self.matrices.push(matrix);
        self
    }

    /// Declare a set of zoom levels, in any order.
    #[must_use]
    pub fn matrices(mut self, matrices: impl IntoIterator<Item = TileMatrix>) -> Self {
        self.matrices.extend(matrices);
        self
    }

    /// Set `gpkg_contents.identifier` (a human-readable name). Defaults to the
    /// table name when left unset.
    #[must_use]
    pub fn identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Set `gpkg_contents.description`.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Allow zoom levels that do not step by factors of two, registering the
    /// `gpkg_zoom_other` extension for the table.
    ///
    /// Off by default, and the omission is an error rather than a silent
    /// registration: a ladder that does not double is usually a mistake in the
    /// pixel sizes, and a file that quietly carries an extension is one whose
    /// readers may not have it. Such a pyramid is always *read* whether or not
    /// this was set, as it is for the file's original writer.
    #[must_use]
    pub fn allow_zoom_other(mut self, allow: bool) -> Self {
        self.allow_zoom_other = allow;
        self
    }

    /// The table name.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

/// A handle to one tile pyramid of a [`GeoPackage`].
///
/// Obtained from [`GeoPackage::create_tile_pyramid`], [`GeoPackage::tiles`] or
/// [`GeoPackage::tile_pyramids`]. The matrix set and the zoom levels are read
/// once at construction and held sorted by zoom level, so addressing a tile
/// costs a binary search rather than a query, and the handle borrows the
/// [`GeoPackage`] for its lifetime.
pub struct TilePyramid<'a> {
    gpkg: &'a GeoPackage,
    table_name: String,
    matrix_set: TileMatrixSet,
    /// Ascending by zoom level, which [`Self::matrix`] binary-searches.
    matrices: Vec<TileMatrix>,
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

impl GeoPackage {
    /// Create a tile pyramid from a [`TilePyramidBuilder`].
    ///
    /// Emits the tile pyramid user table, a `gpkg_contents` row
    /// (`data_type = 'tiles'`, bounded by the matrix set extent), the
    /// `gpkg_tile_matrix_set` row and one `gpkg_tile_matrix` row per zoom
    /// level, in one transaction, then returns a handle to the new pyramid.
    /// `gpkg_tile_matrix_set` and `gpkg_tile_matrix` are created on first use,
    /// as `gpkg_geometry_columns` is for feature layers.
    ///
    /// # Errors
    ///
    /// - [`Error::ReservedTablePrefix`] if the name begins `gpkg_`.
    /// - [`Error::TableAlreadyExists`] if a table or view of that name exists.
    /// - [`Error::UnknownSrs`] if the matrix set's `srs_id` is not registered
    ///   in `gpkg_spatial_ref_sys`.
    /// - [`Error::Tile`] if the pyramid breaks one of the spec's consistency
    ///   rules (Requirements 45 to 53).
    /// - [`Error::ZoomOtherNotEnabled`] if its zoom levels do not step by
    ///   factors of two and [`TilePyramidBuilder::allow_zoom_other`] was not
    ///   set.
    pub fn create_tile_pyramid(&self, builder: &TilePyramidBuilder) -> Result<TilePyramid<'_>> {
        let name = &builder.table_name;
        if name
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gpkg_"))
        {
            return Err(Error::ReservedTablePrefix {
                table_name: name.clone(),
            });
        }
        let conn = self.connection();
        if table_exists(conn, name)? {
            return Err(Error::TableAlreadyExists {
                table_name: name.clone(),
            });
        }
        if self.srs(builder.matrix_set.srs_id)?.is_none() {
            return Err(Error::UnknownSrs {
                srs_id: builder.matrix_set.srs_id,
            });
        }
        builder.matrix_set.validate(&builder.matrices)?;
        let zoom_other = !tiles::is_power_of_two_ladder(&builder.matrices);
        if zoom_other && !builder.allow_zoom_other {
            return Err(Error::ZoomOtherNotEnabled {
                table_name: name.clone(),
            });
        }

        let identifier = builder.identifier.clone().unwrap_or_else(|| name.clone());
        let description = builder.description.clone().unwrap_or_default();
        let set = &builder.matrix_set;

        let tx = conn.unchecked_transaction()?;
        for (exists, sql) in [
            (
                table_exists(&tx, "gpkg_tile_matrix_set")?,
                ddl::CREATE_GPKG_TILE_MATRIX_SET,
            ),
            (
                table_exists(&tx, "gpkg_tile_matrix")?,
                ddl::CREATE_GPKG_TILE_MATRIX,
            ),
        ] {
            if !exists {
                tx.execute_batch(sql)?;
            }
        }
        tx.execute_batch(&tiles::create_tile_table_sql(name)?)?;
        // The extent is the matrix set's, not a measurement: for tiles it is
        // exact by Requirement 144, and gpkg_contents holds the same box.
        tx.execute(
            "INSERT INTO gpkg_contents \
             (table_name, data_type, identifier, description, min_x, min_y, max_x, max_y, srs_id) \
             VALUES (?1, 'tiles', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                name,
                identifier,
                description,
                set.min_x,
                set.min_y,
                set.max_x,
                set.max_y,
                set.srs_id,
            ],
        )?;
        tx.execute(
            "INSERT INTO gpkg_tile_matrix_set (table_name, srs_id, min_x, min_y, max_x, max_y) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![name, set.srs_id, set.min_x, set.min_y, set.max_x, set.max_y],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO gpkg_tile_matrix \
                 (table_name, zoom_level, matrix_width, matrix_height, tile_width, tile_height, \
                  pixel_x_size, pixel_y_size) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for matrix in &builder.matrices {
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
                &tx,
                Some(name),
                Some(tiles::TILE_DATA_COLUMN),
                ZOOM_OTHER_EXTENSION_NAME,
                ZOOM_OTHER_EXTENSION_DEFINITION,
                tiles::TILE_EXTENSION_SCOPE,
            )?;
        }
        tx.commit()?;
        self.tiles(name)
    }

    /// Open a tile pyramid by name.
    ///
    /// Nothing is validated: the matrix set and zoom levels are reported as the
    /// file records them.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchLayer`] if `name` is not in `gpkg_contents`.
    /// - [`Error::WrongDataType`] if it is registered but not as `tiles`.
    /// - [`Error::NoTileMatrixSet`] if its `gpkg_tile_matrix_set` row is
    ///   missing, which leaves its tiles unlocatable.
    pub fn tiles(&self, name: &str) -> Result<TilePyramid<'_>> {
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
        if data_type != "tiles" {
            return Err(Error::WrongDataType {
                table_name: declared_name,
                expected: "tiles",
                found: data_type,
            });
        }
        let table_name = resolve_table_name(conn, &declared_name)?.unwrap_or(declared_name);
        let matrix_set =
            read_matrix_set(conn, &table_name)?.ok_or_else(|| Error::NoTileMatrixSet {
                table_name: table_name.clone(),
            })?;
        let matrices = read_matrices(conn, &table_name)?;
        Ok(TilePyramid {
            gpkg: self,
            table_name,
            matrix_set,
            matrices,
        })
    }

    /// Every tile pyramid in the file, by `gpkg_contents` name.
    pub fn tile_pyramids(&self) -> Result<Vec<TilePyramid<'_>>> {
        if !table_exists(self.connection(), "gpkg_tile_matrix_set")? {
            return Ok(Vec::new());
        }
        let names: Vec<String> = {
            let mut stmt = self.connection().prepare(
                "SELECT c.table_name FROM gpkg_contents c \
                 JOIN gpkg_tile_matrix_set s ON s.table_name = c.table_name COLLATE NOCASE \
                 WHERE c.data_type = 'tiles' ORDER BY c.table_name",
            )?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        names.iter().map(|name| self.tiles(name)).collect()
    }
}

impl<'a> TilePyramid<'a> {
    /// The physical SQLite table name backing this pyramid.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// The [`GeoPackage`] this pyramid belongs to.
    pub fn gpkg(&self) -> &'a GeoPackage {
        self.gpkg
    }

    /// The pyramid's extent and spatial reference system.
    pub fn matrix_set(&self) -> &TileMatrixSet {
        &self.matrix_set
    }

    /// Every declared zoom level, ascending.
    pub fn matrices(&self) -> &[TileMatrix] {
        &self.matrices
    }

    /// The zoom levels this pyramid declares, ascending.
    pub fn zoom_levels(&self) -> Vec<i64> {
        self.matrices.iter().map(|m| m.zoom_level).collect()
    }

    /// The tile matrix for one zoom level, or `None` if the pyramid does not
    /// declare that level.
    pub fn matrix(&self, zoom_level: i64) -> Option<&TileMatrix> {
        self.matrices
            .binary_search_by_key(&zoom_level, |matrix| matrix.zoom_level)
            .ok()
            .and_then(|index| self.matrices.get(index))
    }

    /// Whether this pyramid satisfies the spec's consistency rules.
    ///
    /// Creation checks this, so a pyramid this crate wrote always passes. Worth
    /// asking of one that arrived in a file from elsewhere, since every tile
    /// bound is calculated from values it does not otherwise question.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] carrying the first rule the pyramid breaks.
    pub fn validate(&self) -> Result<()> {
        self.matrix_set.validate(&self.matrices)?;
        Ok(())
    }
}

/// Read a pyramid's `gpkg_tile_matrix_set` row.
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

/// Read a pyramid's `gpkg_tile_matrix` rows, ascending by zoom level.
///
/// A file with no `gpkg_tile_matrix` table at all has no zoom levels, which is
/// an empty pyramid rather than an error: the tiles table may be empty, and the
/// spec requires a row only for a level that holds tiles.
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
