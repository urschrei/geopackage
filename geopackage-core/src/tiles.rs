//! Tile pyramids: the user data table template (spec clause 2.2.8, Annex C),
//! and the vocabulary shared by everything that reads or writes one.
//!
//! A tile pyramid is a `gpkg_contents` row with `data_type = 'tiles'`, one
//! `gpkg_tile_matrix_set` row fixing the pyramid's bounding box and spatial
//! reference system, one `gpkg_tile_matrix` row per zoom level, and a user
//! table holding the tiles themselves. The table definition SQL for the two
//! catalogue tables is in [`crate::ddl`]; the user table is per-pyramid, so it
//! is built here.

use crate::Error;
use crate::ident::quote;

/// The tile payload column of a tile pyramid user data table.
///
/// Fixed by the spec, and the `gpkg_extensions.column_name` value the tile
/// extensions (`gpkg_webp`, `gpkg_zoom_other`) register against.
pub const TILE_DATA_COLUMN: &str = "tile_data";

/// `CREATE TABLE` statement for a tile pyramid user data table, in the form
/// given by Annex C (Requirement 54): an `INTEGER PRIMARY KEY` acting as the
/// rowid alias, the zoom/column/row index, the payload, and the uniqueness
/// constraint over the three index columns.
///
/// The column names are not caller-configurable: unlike a feature table, whose
/// geometry column is named in `gpkg_geometry_columns`, a tile table's shape is
/// fixed by the spec and every reader assumes it.
pub fn create_tile_table_sql(table: &str) -> Result<String, Error> {
    Ok(format!(
        "CREATE TABLE {} (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         zoom_level INTEGER NOT NULL, \
         tile_column INTEGER NOT NULL, \
         tile_row INTEGER NOT NULL, \
         tile_data BLOB NOT NULL, \
         UNIQUE (zoom_level, tile_column, tile_row))",
        quote(table)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_table_ddl() {
        assert_eq!(
            create_tile_table_sql("basemap").unwrap(),
            "CREATE TABLE \"basemap\" (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             zoom_level INTEGER NOT NULL, \
             tile_column INTEGER NOT NULL, \
             tile_row INTEGER NOT NULL, \
             tile_data BLOB NOT NULL, \
             UNIQUE (zoom_level, tile_column, tile_row))"
        );
    }

    #[test]
    fn quotes_awkward_identifiers() {
        assert!(
            create_tile_table_sql("we\"ird")
                .unwrap()
                .starts_with("CREATE TABLE \"we\"\"ird\" (")
        );
        create_tile_table_sql("").unwrap_err();
    }
}
