//! Rewriting stored values that this crate reads but that do not have the
//! form the spec gives them: [`Layer::repair_wkb_type_codes`].
//!
//! Each repair rewrites only the rows that need it, in one transaction, and
//! returns how many it rewrote. A row that the repair cannot read is left as
//! it is: [`crate::GeoPackage::validate`] reports it separately. No repair is
//! ever made automatically.

use geopackage_core::curve;
use geopackage_core::geometry::{self, GeometryError};
use geopackage_core::gpb;
use geopackage_core::ident::quote;
use rusqlite::types::ValueRef;

use crate::transaction::WriteTransaction;
use crate::{Error, Layer, Result};

impl Layer<'_> {
    /// Rewrites each geometry whose WKB gives Z or M by a high-bit flag in a
    /// type code, at any nesting depth, with ISO type codes. Returns how many
    /// geometries it rewrote.
    ///
    /// A GPB body must be ISO WKB (see
    /// [`curve::BodyScan::flagged_type_code`]). The rewrite keeps the
    /// coordinates, the byte order and the header's SRS id, and writes the
    /// header that [`geometry::encode_gpb_from_wkb`] writes, envelope
    /// included. The RTree triggers keep the spatial index in step.
    ///
    /// A geometry whose header or body cannot be read is left as it is.
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] on a layer with no geometry column.
    /// - [`Error::NoPrimaryKey`] if the table has no single-column primary key.
    /// - [`Error::UnsupportedExtension`] if an extension on the table blocks
    ///   writes.
    pub fn repair_wkb_type_codes(&self) -> Result<usize> {
        self.gpkg().check_writable(self.table_name())?;
        let column = quote(&self.require_geometry_column()?.column_name)?;
        let key = quote(self.require_primary_key()?)?;
        let table = quote(self.table_name())?;
        let conn = self.gpkg().connection();
        let tx = WriteTransaction::begin(conn)?;

        // Two passes, so that no row is written while the scan that finds it
        // is still open on the same table: first the keys of the rows to
        // rewrite, then each row on its own.
        let mut keys = Vec::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT {key}, {column} FROM {table} WHERE {column} IS NOT NULL"
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if let ValueRef::Blob(blob) = row.get_ref(1)?
                    && matches!(flagged_body(blob), Ok(Some(_)))
                {
                    keys.push(row.get::<_, i64>(0)?);
                }
            }
        }

        let mut select = conn.prepare(&format!("SELECT {column} FROM {table} WHERE {key} = ?1"))?;
        let mut update = conn.prepare(&format!(
            "UPDATE {table} SET {column} = ?1 WHERE {key} = ?2"
        ))?;
        for key in &keys {
            let blob: Vec<u8> = select.query_row([key], |row| row.get(0))?;
            let rewritten = rewrite_as_iso(&blob).map_err(|e| Error::Core(e.into()))?;
            update.execute(rusqlite::params![rewritten, key])?;
        }
        drop((select, update));
        tx.commit()?;
        Ok(keys.len())
    }
}

/// Returns the first flagged type code in a GPB blob's body, or `None` when
/// every code is ISO.
fn flagged_body(blob: &[u8]) -> std::result::Result<Option<u32>, GeometryError> {
    let offset = gpb::body_offset(blob)?;
    Ok(curve::scan(blob.get(offset..).unwrap_or_default())?.flagged_type_code)
}

/// Returns a GPB blob for the same geometry as `blob`, with ISO type codes and
/// the header's SRS id.
fn rewrite_as_iso(blob: &[u8]) -> std::result::Result<Vec<u8>, GeometryError> {
    let (header, offset) = gpb::parse_header(blob)?;
    let body = curve::to_iso_wkb(blob.get(offset..).unwrap_or_default())?;
    Ok(geometry::encode_gpb_from_wkb(&body, header.srs_id)?.blob)
}
