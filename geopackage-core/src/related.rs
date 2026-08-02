//! The Related Tables Extension's model (OGC 18-000): relationships between a
//! base table and a table of related content, through a mapping table.
//!
//! One catalogue table, `gpkgext_relations`, names each relationship; each row
//! points at a user-defined mapping table whose `base_id` and `related_id`
//! columns store the pairs. The mapping table is the relationship's data, and
//! this crate neither constrains nor infers its cardinality: the spec is
//! explicit that a `UNIQUE` constraint could enforce one-to-many but is NOT
//! RECOMMENDED, because SQLite does not expose such constraints in an easily
//! queryable way.
//!
//! 18-000 has exactly one published version, 1.0, approved 2019-03-26. The
//! `related_tables` and `gpkg_related_tables` spellings are not two versions:
//! the document names the first and sanctions the second as an alias, and its
//! own abstract test suite queries for both.

use std::fmt;

use crate::ident;

/// Registered extension name.
///
/// The spec's Extension Template gives `related_tables` and adds that "upon
/// adoption the alias `gpkg_related_tables` MAY be used". This crate writes
/// the prefixed form, the spelling found in files in circulation, GDAL's
/// among them; both are recognised on read, and the spec's own tests accept
/// either.
pub const EXTENSION_NAME: &str = "gpkg_related_tables";
/// `gpkg_extensions.definition` value for [`EXTENSION_NAME`].
pub const EXTENSION_DEFINITION: &str = "http://www.geopackage.org/18-000.html";
/// `gpkg_extensions.scope` value for [`EXTENSION_NAME`].
pub const EXTENSION_SCOPE: &str = "read-write";

/// The extension's catalogue table.
pub const RELATIONS_TABLE: &str = "gpkgext_relations";

/// `gpkgext_relations`, verbatim from the spec's Extended Relations Table
/// Definition SQL.
pub const CREATE_GPKGEXT_RELATIONS: &str = "\
CREATE TABLE 'gpkgext_relations' (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  base_table_name TEXT NOT NULL,
  base_primary_column TEXT NOT NULL DEFAULT 'id',
  related_table_name TEXT NOT NULL,
  related_primary_column TEXT NOT NULL DEFAULT 'id',
  relation_name TEXT NOT NULL,
  mapping_table_name TEXT NOT NULL UNIQUE
)";

/// A row of `gpkgext_relations`: one relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The row's `id`.
    pub id: i64,
    /// The table the relationship starts from.
    pub base_table_name: String,
    /// The base table's primary key column. The table's default is `id`.
    pub base_primary_column: String,
    /// The table of related content.
    pub related_table_name: String,
    /// The related table's primary key column. The table's default is `id`.
    pub related_primary_column: String,
    /// What kind of relationship this is.
    pub relation_name: RelationName,
    /// The mapping table that stores the pairs.
    pub mapping_table_name: String,
}

/// A relationship's kind (`relation_name`).
///
/// Requirement 8: the value SHALL either name one of the requirements classes
/// defined in this or another OGC standard, or be of the form
/// `x-<author>_<relation_name>`. A value that is neither is non-conformant, and
/// is kept as [`RelationName::Other`] so such a file still reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RelationName {
    /// Multimedia content. The related table is an attributes table.
    Media,
    /// Simple attributes: text, numeric and boolean columns only.
    SimpleAttributes,
    /// Related features.
    Features,
    /// Related attributes.
    Attributes,
    /// Related tiles.
    Tiles,
    /// An `x-<author>_<name>` extension, stored without the `x-` prefix.
    Extended {
        /// The person or organisation maintaining the relation type.
        author: String,
        /// The relation name within that author's namespace.
        name: String,
    },
    /// A value that is neither a defined class nor the `x-` form, kept as
    /// written. Non-conformant per Requirement 8.
    Other(String),
}

impl RelationName {
    /// Identifies a `relation_name` value. Never fails.
    pub fn parse(value: &str) -> Self {
        match value {
            "media" => return Self::Media,
            "simple_attributes" => return Self::SimpleAttributes,
            "features" => return Self::Features,
            "attributes" => return Self::Attributes,
            "tiles" => return Self::Tiles,
            _ => {}
        }
        // `x-<author>_<relation_name>`: the author runs to the first
        // underscore, and the rest is the name.
        if let Some(rest) = value.strip_prefix("x-")
            && let Some((author, name)) = rest.split_once('_')
            && !author.is_empty()
            && !name.is_empty()
        {
            return Self::Extended {
                author: author.to_owned(),
                name: name.to_owned(),
            };
        }
        Self::Other(value.to_owned())
    }

    /// Returns the `relation_name` column value.
    pub fn as_string(&self) -> String {
        match self {
            Self::Media => "media".to_owned(),
            Self::SimpleAttributes => "simple_attributes".to_owned(),
            Self::Features => "features".to_owned(),
            Self::Attributes => "attributes".to_owned(),
            Self::Tiles => "tiles".to_owned(),
            Self::Extended { author, name } => format!("x-{author}_{name}"),
            Self::Other(value) => value.clone(),
        }
    }

    /// Returns `true` if Requirement 8 accepts this value.
    pub fn is_conformant(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

impl fmt::Display for RelationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

/// Returns `CREATE TABLE` SQL for a user-defined mapping table.
///
/// Requirement 9 asks for `base_id` and `related_id` and permits other
/// columns; Table 3 gives both as non-null. GDAL leaves them nullable, which
/// is accepted on read, but this crate writes the table definition's form.
///
/// # Errors
///
/// [`crate::Error::InvalidIdentifier`] if `table_name` cannot be quoted.
pub fn create_mapping_table_sql(table_name: &str) -> Result<String, crate::Error> {
    Ok(format!(
        "CREATE TABLE {} (\n  base_id INTEGER NOT NULL,\n  related_id INTEGER NOT NULL\n)",
        ident::quote(table_name)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_requirements_classes_round_trip() {
        for name in [
            RelationName::Media,
            RelationName::SimpleAttributes,
            RelationName::Features,
            RelationName::Attributes,
            RelationName::Tiles,
        ] {
            assert_eq!(RelationName::parse(&name.as_string()), name);
            assert!(name.is_conformant());
        }
    }

    #[test]
    fn the_extended_form_round_trips_and_splits_author_from_name() {
        let parsed = RelationName::parse("x-acme_inspections");
        assert_eq!(
            parsed,
            RelationName::Extended {
                author: "acme".to_owned(),
                name: "inspections".to_owned(),
            }
        );
        assert_eq!(parsed.as_string(), "x-acme_inspections");
        assert!(parsed.is_conformant());

        // The name keeps any further underscores; only the first splits.
        assert_eq!(
            RelationName::parse("x-acme_site_visits").as_string(),
            "x-acme_site_visits"
        );
    }

    #[test]
    fn a_value_requirement_8_rejects_is_kept_rather_than_lost() {
        for value in ["photos", "x-", "x-acme", "x-_name", "x-acme_"] {
            let parsed = RelationName::parse(value);
            assert_eq!(parsed, RelationName::Other(value.to_owned()), "{value}");
            assert_eq!(parsed.as_string(), value);
            assert!(!parsed.is_conformant(), "{value}");
        }
    }

    #[test]
    fn mapping_table_sql_quotes_its_name_and_writes_both_columns() {
        let sql = create_mapping_table_sql("odd\"name").unwrap();
        assert!(sql.starts_with("CREATE TABLE \"odd\"\"name\" ("), "{sql}");
        assert!(sql.contains("base_id INTEGER NOT NULL"));
        assert!(sql.contains("related_id INTEGER NOT NULL"));
    }
}
