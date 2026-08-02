use std::collections::HashMap;
use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use geopackage_core::types::ColumnType;

use crate::schema::{Column, GeometryColumn};
use crate::{Layer, Result};

/// The Arrow extension-name metadata key, from the Arrow columnar spec.
pub(crate) const EXTENSION_NAME_KEY: &str = "ARROW:extension:name";

/// The Arrow extension-metadata key, which GeoArrow uses to carry CRS and
/// related information as JSON.
pub(crate) const EXTENSION_METADATA_KEY: &str = "ARROW:extension:metadata";

/// The GeoArrow extension name for a WKB-encoded geometry column.
pub(crate) const GEOARROW_WKB: &str = "geoarrow.wkb";

/// The time unit `DATETIME` columns are represented in.
pub(crate) const DATETIME_UNIT: TimeUnit = TimeUnit::Microsecond;

impl Layer<'_> {
    /// The Arrow schema this layer's rows are read into.
    ///
    /// Fields appear in the table's column order, so the geometry column sits
    /// where it sits in the table rather than being moved to one end. See the
    /// [module documentation](super) for the type mapping.
    ///
    /// A projection ([`Layer::with_columns`], [`Layer::without_geometry`])
    /// narrows this schema exactly as it narrows a row read: the primary key
    /// is always a field, a value column is a field when the projection names
    /// it, and the geometry is a field only when projected in. The reads on
    /// this handle return batches of this schema.
    ///
    /// # Errors
    ///
    /// [`crate::Error`] if the table schema cannot be introspected.
    pub fn arrow_schema(&self) -> Result<SchemaRef> {
        let geometry = self.geometry_column();
        let fields: Vec<Field> = self
            .schema()
            .columns
            .iter()
            .filter(|column| self.arrow_reads_column(column, geometry))
            .map(|column| field_for(column, geometry))
            .collect();
        Ok(Arc::new(Schema::new(fields)))
    }

    /// Whether a read of this handle carries `column`: the primary key always,
    /// the geometry per the projection, a value column when the projection
    /// keeps it.
    fn arrow_reads_column(&self, column: &Column, geometry: Option<&GeometryColumn>) -> bool {
        if self.primary_key_column() == Some(column.name.as_str()) {
            return true;
        }
        if geometry.is_some_and(|g| g.column_name == column.name) {
            return self.reads_geometry();
        }
        self.read_value_columns()
            .iter()
            .any(|kept| kept.name == column.name)
    }
}

/// The Arrow field for one table column.
fn field_for(column: &Column, geometry: Option<&GeometryColumn>) -> Field {
    let is_geometry = geometry.is_some_and(|g| g.column_name == column.name);
    if is_geometry {
        // Unwrap-free: `is_geometry` is only true when `geometry` is `Some`.
        let srs_id = geometry.map_or(0, |g| g.srs_id);
        return geometry_field(&column.name, column.not_null, srs_id);
    }
    // An `INTEGER PRIMARY KEY` is SQLite's rowid alias and can never be NULL,
    // whether or not the column carries an explicit NOT NULL.
    let nullable = !column.not_null && !column.is_primary_key();
    Field::new(&column.name, data_type_for(column), nullable)
}

/// The Arrow data type for a non-geometry column.
fn data_type_for(column: &Column) -> DataType {
    let Some(declared) = &column.column_type else {
        return affinity_type(&column.declared_type);
    };
    match declared {
        ColumnType::Boolean => DataType::Boolean,
        ColumnType::TinyInt
        | ColumnType::SmallInt
        | ColumnType::MediumInt
        | ColumnType::Integer => DataType::Int64,
        ColumnType::Float | ColumnType::Double => DataType::Float64,
        ColumnType::Text(_) => DataType::Utf8,
        ColumnType::Blob(_) => DataType::Binary,
        ColumnType::Date => DataType::Date32,
        ColumnType::DateTime => DataType::Timestamp(DATETIME_UNIT, Some("UTC".into())),
        // A geometry type name on a column that is not *the* geometry column,
        // which a conformant file does not have. Its blob is surfaced as one.
        ColumnType::Geometry(_) => DataType::Binary,
        // `ColumnType` is `#[non_exhaustive]`: a type added to the spec later
        // is mapped by affinity rather than crashing.
        _ => affinity_type(&column.declared_type),
    }
}

/// The Arrow type for a declared type outside the spec vocabulary, following
/// SQLite's column-affinity rules.
///
/// The order of the tests is part of the rule and is not alphabetical: `INT`
/// wins over everything, and the `CHAR`/`CLOB`/`TEXT` family wins over `BLOB`.
///
/// SQLite's last two rules, REAL affinity (`REAL`, `FLOA`, `DOUB`) and the
/// NUMERIC fall-through, are one branch here because both land on `Float64`.
/// For REAL that is exact. NUMERIC is the one lossy case in this function:
/// SQLite may store an integer in such a column, and an integer beyond 2^53
/// does not survive the conversion. The alternative is surfacing the column as
/// text, which is lossless and unusable, so the loss is taken and recorded.
fn affinity_type(declared: &str) -> DataType {
    let declared = declared.to_ascii_uppercase();
    let has = |needle: &str| declared.contains(needle);
    if has("INT") {
        DataType::Int64
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        DataType::Utf8
    } else if has("BLOB") || declared.is_empty() {
        DataType::Binary
    } else {
        DataType::Float64
    }
}

/// The geometry field: WKB bytes carrying the GeoArrow extension name, and the
/// CRS as extension metadata.
///
/// The bytes are the GPB blob's WKB body, which is why this encoding is the one
/// to start from: it needs no geometry parsing at all.
fn geometry_field(name: &str, not_null: bool, srs_id: i32) -> Field {
    let mut metadata = HashMap::new();
    metadata.insert(EXTENSION_NAME_KEY.to_owned(), GEOARROW_WKB.to_owned());
    metadata.insert(EXTENSION_METADATA_KEY.to_owned(), crs_metadata(srs_id));
    Field::new(name, DataType::Binary, !not_null).with_metadata(metadata)
}

/// The GeoArrow extension metadata for a geometry column: a JSON object whose
/// `crs` names the SRS.
///
/// PROJJSON is what GeoArrow prefers, because an authority code leaves the
/// reader to resolve it against a registry it may not have, so that is what
/// this emits wherever the EPSG registry has a definition. A `srs_id` the
/// registry does not know falls back to an `EPSG:<code>` authority string,
/// which GeoArrow permits. `srs_id` 0 and -1 are the spec's undefined values
/// and carry no CRS at all.
fn crs_metadata(srs_id: i32) -> String {
    if srs_id <= 0 {
        return "{}".to_owned();
    }
    // GeoArrow recommends PROJJSON and says an authority code "should only be
    // used as a last resort", because it leaves the reader to resolve the code
    // against a registry it may not have. Emit the full definition when we can
    // and keep the code as the fallback for anything outside the EPSG registry
    // (a user-defined srs_id, say).
    epsg_utils::epsg_to_projjson(srs_id).map_or_else(
        |_| format!(r#"{{"crs":"EPSG:{srs_id}","crs_type":"authority_code"}}"#),
        |projjson| format!(r#"{{"crs":{projjson},"crs_type":"projjson"}}"#),
    )
}

/// The numeric part of an `EPSG:<code>` authority code in GeoArrow CRS
/// metadata, if that is the form it takes.
///
/// Handles both forms [`crs_metadata`] emits, and the same two forms from any
/// other producer: a PROJJSON object, or an `EPSG:<code>` authority string.
///
/// PROJJSON needs a real parse rather than a scan for the first `"code"`. A
/// CRS object nests identifiers for its coordinate system, datum and
/// ellipsoid, all of which have EPSG codes of their own: in EPSG:4326 the
/// first one to appear is 6422, the ellipsoidal coordinate system. Only the
/// top-level `id` identifies the CRS itself. Anything else yields `None` and
/// the caller sets the SRS themselves.
pub(crate) fn epsg_code(metadata: &str) -> Option<i32> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let crs = value.get("crs")?;
    if let Some(id) = crs.get("id")
        && id
            .get("authority")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|a| a.eq_ignore_ascii_case("EPSG"))
    {
        return id.get("code")?.as_i64()?.try_into().ok();
    }
    let code = crs.as_str()?.strip_prefix("EPSG:")?;
    code.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epsg_code_reads_the_crs_id_not_a_nested_one() {
        // EPSG:4326's PROJJSON nests the ellipsoidal coordinate system's own
        // identifier, 6422, ahead of the CRS's. A scan for the first code
        // would return that.
        let metadata = crs_metadata(4326);
        assert!(
            metadata.contains(r#""code":6422"#),
            "the trap this guards against has moved, update the test: {metadata}"
        );
        assert_eq!(epsg_code(&metadata), Some(4326));
    }

    #[test]
    fn epsg_code_reads_the_authority_code_form() {
        // What we emit for a code the registry does not know, and a form other
        // producers use.
        assert_eq!(
            epsg_code(r#"{"crs":"EPSG:27700","crs_type":"authority_code"}"#),
            Some(27700)
        );
    }

    #[test]
    fn epsg_code_declines_what_it_cannot_identify() {
        assert_eq!(epsg_code("{}"), None);
        assert_eq!(epsg_code("not json"), None);
        // A CRS defined by some other authority is not an EPSG code.
        assert_eq!(
            epsg_code(r#"{"crs":{"id":{"authority":"ESRI","code":104305}}}"#),
            None
        );
    }

    #[test]
    fn a_layer_srs_round_trips_through_the_metadata() {
        for code in [4326, 27700, 32630, 4979] {
            assert_eq!(epsg_code(&crs_metadata(code)), Some(code), "code {code}");
        }
    }

    #[test]
    fn affinity_follows_sqlite_rules() {
        // The examples SQLite's own documentation gives for each affinity.
        assert_eq!(affinity_type("VARCHAR(20)"), DataType::Utf8);
        assert_eq!(affinity_type("NVARCHAR(100)"), DataType::Utf8);
        assert_eq!(affinity_type("CLOB"), DataType::Utf8);
        assert_eq!(affinity_type("BIGINT"), DataType::Int64);
        assert_eq!(affinity_type("UNSIGNED BIG INT"), DataType::Int64);
        assert_eq!(affinity_type(""), DataType::Binary);
        assert_eq!(affinity_type("DOUBLE PRECISION"), DataType::Float64);
        assert_eq!(affinity_type("NUMERIC"), DataType::Float64);
        assert_eq!(affinity_type("DECIMAL(10,5)"), DataType::Float64);
    }

    #[test]
    fn int_wins_over_the_text_family() {
        // SQLite's rule 1 is tested before rule 2, so a type containing both
        // takes INTEGER affinity. `INT` inside `POINT` is the reason the read
        // path never routes a geometry column through here.
        assert_eq!(affinity_type("INTCHAR"), DataType::Int64);
    }

    #[test]
    fn char_family_wins_over_blob() {
        assert_eq!(affinity_type("TEXTBLOB"), DataType::Utf8);
    }
}
