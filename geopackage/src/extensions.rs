//! `gpkg_extensions`: the one place this crate writes the extension catalogue,
//! and the queries that ask what a file has registered.
//!
//! Every extension row carries the same five values, so the differences between
//! extensions live in the constants their own modules hold, not in repeated
//! insert statements. The table is created on first registration and never
//! dropped: an empty `gpkg_extensions` is legal, and a file that once carried
//! an extension keeps the table after the extension is removed, as
//! [`crate::Layer::drop_spatial_index`] does.

use geopackage_core::ddl;
use rusqlite::Connection;

use crate::{Result, table_exists};

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
