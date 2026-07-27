//! `gpkg_extensions`: the catalogue of extensions a file declares, the one
//! place this crate writes to it, and what this crate can do with each row.
//!
//! Every extension row carries the same five values, so the differences
//! between extensions live in the constants their own modules hold, not in
//! repeated insert statements. The table is created on first registration and
//! never dropped: an empty `gpkg_extensions` is legal, and a file that once
//! carried an extension keeps the table after the extension is removed, as
//! [`crate::Layer::drop_spatial_index`] does.
//!
//! Reading the catalogue is what lets a caller "fail fast", which is the
//! purpose the spec gives the table: an application "can query the
//! `gpkg_extensions` table instead of the contents of all the user data tables
//! to determine if it has the required capabilities to read or write to tables
//! with extensions, and to 'fail fast' and return an error message if it does
//! not" (clause 2.3.2). [`ExtensionRow::support`] is this crate's answer to
//! that question for one row.

use geopackage_core::ddl;
use geopackage_core::extensions::{Extension, ExtensionScope, ExtensionSupport};
use rusqlite::{Connection, OptionalExtension};

use crate::{GeoPackage, Layer, Result, TilePyramid, table_exists};

/// A row of `gpkg_extensions`: one extension, and what it applies to.
///
/// The five columns are fixed by Requirement 58, which also forbids an
/// extension from changing them, so this mirrors the table exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRow {
    /// The table the extension applies to, or `None` when it applies to the
    /// whole GeoPackage.
    ///
    /// Requirement 60: this may name a table in `gpkg_contents`, or a new
    /// table the extension itself requires, such as `gpkg_metadata`.
    pub table_name: Option<String>,
    /// The column the extension applies to, or `None` when it applies to the
    /// whole table (Requirement 61).
    pub column_name: Option<String>,
    /// The `extension_name` value, exactly as the file spells it.
    ///
    /// Case sensitive, of the form `<author>_<extension name>`
    /// (Requirement 62). Use [`ExtensionRow::extension`] to identify it.
    pub name: String,
    /// A permalink, URI, or reference to the document defining the extension
    /// (Requirement 63).
    pub definition: String,
    /// What the extension affects (Requirement 64).
    pub scope: ExtensionScope,
}

impl ExtensionRow {
    /// Identify [`ExtensionRow::name`].
    pub fn extension(&self) -> Extension {
        Extension::from_name(&self.name)
    }

    /// What this crate can do with the extension.
    pub fn support(&self) -> ExtensionSupport {
        self.extension().support()
    }
}

/// The `SELECT` behind every catalogue query, in a stable order.
const SELECT_ROWS: &str = "SELECT table_name, column_name, extension_name, definition, scope \
     FROM gpkg_extensions";

fn read_rows(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<ExtensionRow>> {
    if !table_exists(conn, "gpkg_extensions")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| {
        Ok(ExtensionRow {
            table_name: r.get(0)?,
            column_name: r.get(1)?,
            name: r.get(2)?,
            definition: r.get(3)?,
            scope: ExtensionScope::parse(&r.get::<_, String>(4)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

impl GeoPackage {
    /// Every row of `gpkg_extensions`, ordered by extension name then by what
    /// it applies to.
    ///
    /// Empty for a file with no `gpkg_extensions` table, which Requirement 59
    /// defines as a GeoPackage rather than an Extended GeoPackage, and empty
    /// for a file whose table has no rows, which means the same thing.
    pub fn extensions(&self) -> Result<Vec<ExtensionRow>> {
        read_rows(
            self.connection(),
            &format!("{SELECT_ROWS} ORDER BY extension_name, table_name, column_name"),
            &[],
        )
    }

    /// The rows that apply to one table.
    ///
    /// Rows with a NULL `table_name` apply to the whole GeoPackage rather than
    /// to any table, so they are not included here; [`GeoPackage::extensions`]
    /// has them. Table names are compared case-insensitively, as the note
    /// under Requirement 60 asks: SQLite table names are not case sensitive,
    /// and `gpkg_extensions` need not agree with `sqlite_master` on the case.
    pub fn table_extensions(&self, table_name: &str) -> Result<Vec<ExtensionRow>> {
        read_rows(
            self.connection(),
            &format!(
                "{SELECT_ROWS} WHERE lower(table_name) = lower(?1) \
                 ORDER BY extension_name, column_name"
            ),
            &[&table_name],
        )
    }
}

impl Layer<'_> {
    /// The extensions registered against this layer's table.
    ///
    /// See [`GeoPackage::table_extensions`], which this calls: an indexed
    /// layer has a `gpkg_rtree_index` row here, and a layer carrying a
    /// non-linear geometry type has a `gpkg_geom_<TYPE>` row.
    pub fn extensions(&self) -> Result<Vec<ExtensionRow>> {
        self.gpkg().table_extensions(self.table_name())
    }
}

impl TilePyramid<'_> {
    /// The extensions registered against this pyramid's table.
    ///
    /// See [`GeoPackage::table_extensions`], which this calls: a pyramid
    /// holding WebP payloads has a `gpkg_webp` row here, and one whose zoom
    /// levels do not step by factors of two has a `gpkg_zoom_other` row.
    pub fn extensions(&self) -> Result<Vec<ExtensionRow>> {
        self.gpkg().table_extensions(self.table_name())
    }
}

/// Register an extension, creating `gpkg_extensions` on first use.
///
/// `table` and `column` are `None` for an extension scoped to the whole
/// GeoPackage rather than to one table or column. Runs on the connection or
/// transaction it is given and commits nothing.
pub(crate) fn register(
    conn: &Connection,
    table: Option<&str>,
    column: Option<&str>,
    name: &str,
    definition: &str,
    scope: &str,
) -> Result<()> {
    if !table_exists(conn, "gpkg_extensions")? {
        conn.execute_batch(ddl::CREATE_GPKG_EXTENSIONS)?;
    }
    conn.execute(
        "INSERT INTO gpkg_extensions \
         (table_name, column_name, extension_name, definition, scope) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![table, column, name, definition, scope],
    )?;
    Ok(())
}

/// Whether `name` is registered for `table`, which is `None` for an extension
/// scoped to the whole GeoPackage.
///
/// `false` for a file with no `gpkg_extensions` table at all, which is the
/// common case rather than an error.
pub(crate) fn is_registered(conn: &Connection, table: Option<&str>, name: &str) -> Result<bool> {
    if !table_exists(conn, "gpkg_extensions")? {
        return Ok(false);
    }
    Ok(conn
        .query_row(
            "SELECT 1 FROM gpkg_extensions WHERE extension_name = ?1 AND table_name IS ?2",
            rusqlite::params![name, table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Remove an extension's registration for a table and column.
///
/// A no-op on a file with no `gpkg_extensions` table, or no such row.
pub(crate) fn unregister(conn: &Connection, table: &str, column: &str, name: &str) -> Result<()> {
    if !table_exists(conn, "gpkg_extensions")? {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM gpkg_extensions \
         WHERE table_name = ?1 AND column_name = ?2 AND extension_name = ?3",
        rusqlite::params![table, column, name],
    )?;
    Ok(())
}
