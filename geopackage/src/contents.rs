//! The `gpkg_contents` table: one row for each user data table.

use crate::{GeoPackage, Result};

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
    /// Tiled gridded coverage, a tile pyramid whose tiles hold measurements
    /// instead of images (`2d-gridded-coverage`, OGC 17-066r2
    /// Requirement 5).
    ///
    /// Open it with [`GeoPackage::coverage`], not with [`GeoPackage::tiles`].
    Coverage,
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
            geopackage_core::coverage::COVERAGE_DATA_TYPE => Self::Coverage,
            "attributes" => Self::Attributes,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl GeoPackage {
    /// Returns the rows of `gpkg_contents`.
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
}
