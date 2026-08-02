//! The Related Tables Extension (OGC 18-000): `gpkgext_relations` and the
//! user-defined mapping tables it points at.
//!
//! Reading works for any relation type, including ones this crate has never
//! heard of: a relationship is a base table, a related table and a mapping
//! table, and walking it needs nothing more. Writing is offered for the
//! requirements classes the spec defines.
//!
//! Cardinality is not modelled. The spec chooses not to constrain it and warns
//! against enforcing one-to-many with a `UNIQUE` constraint, because SQLite
//! does not expose such constraints in an easily queryable way, so
//! [`GeoPackage::related_ids`] returns whatever pairs the mapping table
//! contains.

use geopackage_core::ident::quote;
use geopackage_core::related::{
    CREATE_GPKGEXT_RELATIONS, EXTENSION_DEFINITION, EXTENSION_NAME, EXTENSION_SCOPE,
    RELATIONS_TABLE, Relation, RelationName, create_mapping_table_sql,
};
use rusqlite::OptionalExtension;

use crate::transaction::WriteTransaction;
use crate::{Error, GeoPackage, Result, table_exists};

/// A relationship to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRelation {
    /// The table the relationship starts from. Must be in `gpkg_contents`.
    pub base_table_name: String,
    /// The table of related content. Must be in `gpkg_contents`.
    pub related_table_name: String,
    /// What kind of relationship this is.
    pub relation_name: RelationName,
    /// The mapping table to create. Must not already exist.
    pub mapping_table_name: String,
    /// The base table's primary key column. `id` when `None`, which is the
    /// column default, though a GeoPackage feature table conventionally uses
    /// `fid`.
    pub base_primary_column: Option<String>,
    /// The related table's primary key column. `id` when `None`.
    pub related_primary_column: Option<String>,
}

impl NewRelation {
    /// A relationship between two tables through a mapping table, with both
    /// primary key columns defaulting to `id`.
    pub fn new(
        base_table_name: impl Into<String>,
        related_table_name: impl Into<String>,
        relation_name: RelationName,
        mapping_table_name: impl Into<String>,
    ) -> Self {
        Self {
            base_table_name: base_table_name.into(),
            related_table_name: related_table_name.into(),
            relation_name,
            mapping_table_name: mapping_table_name.into(),
            base_primary_column: None,
            related_primary_column: None,
        }
    }

    /// Name the base table's primary key column, when it is not `id`.
    #[must_use]
    pub fn base_primary_column(mut self, column: impl Into<String>) -> Self {
        self.base_primary_column = Some(column.into());
        self
    }

    /// Name the related table's primary key column, when it is not `id`.
    #[must_use]
    pub fn related_primary_column(mut self, column: impl Into<String>) -> Self {
        self.related_primary_column = Some(column.into());
        self
    }
}

impl GeoPackage {
    /// Returns every `gpkgext_relations` row.
    ///
    /// An empty vector for a file without the extension.
    ///
    /// # Errors
    ///
    /// [`Error`] if the table cannot be read.
    pub fn relations(&self) -> Result<Vec<Relation>> {
        let conn = self.connection();
        if !table_exists(conn, RELATIONS_TABLE)? {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT id, base_table_name, base_primary_column, related_table_name, \
                    related_primary_column, relation_name, mapping_table_name \
             FROM gpkgext_relations ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let relation_name: String = row.get(5)?;
                Ok(Relation {
                    id: row.get(0)?,
                    base_table_name: row.get(1)?,
                    base_primary_column: row.get(2)?,
                    related_table_name: row.get(3)?,
                    related_primary_column: row.get(4)?,
                    relation_name: RelationName::parse(&relation_name),
                    mapping_table_name: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Returns the relationships whose base table is `table_name`.
    ///
    /// # Errors
    ///
    /// As [`GeoPackage::relations`].
    pub fn relations_from(&self, table_name: &str) -> Result<Vec<Relation>> {
        Ok(self
            .relations()?
            .into_iter()
            .filter(|relation| relation.base_table_name.eq_ignore_ascii_case(table_name))
            .collect())
    }

    /// The `related_id` values a relationship maps `base_id` to.
    ///
    /// Returns the mapping table's rows as stored, in its own order, with
    /// duplicates
    /// kept: the spec constrains neither the cardinality nor the uniqueness of
    /// a mapping, so removing anything here would be inventing a rule.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchTable`] if the mapping table named by the relationship is
    /// absent, which Requirement 7 says it should not be.
    pub fn related_ids(&self, relation: &Relation, base_id: i64) -> Result<Vec<i64>> {
        let conn = self.connection();
        if !table_exists(conn, &relation.mapping_table_name)? {
            return Err(Error::NoSuchTable {
                table_name: relation.mapping_table_name.clone(),
            });
        }
        let sql = format!(
            "SELECT related_id FROM {} WHERE base_id = ?1",
            quote(&relation.mapping_table_name).map_err(Error::Core)?
        );
        let mut stmt = conn.prepare(&sql)?;
        let ids = stmt
            .query_map([base_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Creates a relationship: its `gpkgext_relations` row, its mapping
    /// table,
    /// and the extension registrations both need.
    ///
    /// Returns the new `gpkgext_relations.id`.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchTable`] if either the base or the related table is not
    ///   in `gpkg_contents` (Requirements 5 and 6).
    /// - [`Error::TableAlreadyExists`] if the mapping table already exists.
    /// - [`Error::NonConformantRelationName`] if `relation_name` is neither a
    ///   defined requirements class nor the `x-<author>_<name>` form
    ///   (Requirement 8).
    pub fn add_relation(&self, relation: &NewRelation) -> Result<i64> {
        if !relation.relation_name.is_conformant() {
            return Err(Error::NonConformantRelationName {
                relation_name: relation.relation_name.as_string(),
            });
        }

        let conn = self.connection();
        let tx = WriteTransaction::begin(conn)?;

        // Requirements 5 and 6: both ends are in gpkg_contents.
        for table in [&relation.base_table_name, &relation.related_table_name] {
            let known: Option<String> = conn
                .query_row(
                    "SELECT table_name FROM gpkg_contents WHERE table_name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_none() {
                return Err(Error::NoSuchTable {
                    table_name: table.clone(),
                });
            }
        }
        if table_exists(conn, &relation.mapping_table_name)? {
            return Err(Error::TableAlreadyExists {
                table_name: relation.mapping_table_name.clone(),
            });
        }

        if !table_exists(conn, RELATIONS_TABLE)? {
            conn.execute_batch(CREATE_GPKGEXT_RELATIONS)?;
            crate::extensions::register(
                conn,
                Some(RELATIONS_TABLE),
                None,
                EXTENSION_NAME,
                EXTENSION_DEFINITION,
                EXTENSION_SCOPE,
            )?;
        }

        conn.execute_batch(
            &create_mapping_table_sql(&relation.mapping_table_name).map_err(Error::Core)?,
        )?;
        // Requirement 3: the mapping table gets its own gpkg_extensions row.
        crate::extensions::register(
            conn,
            Some(&relation.mapping_table_name),
            None,
            EXTENSION_NAME,
            EXTENSION_DEFINITION,
            EXTENSION_SCOPE,
        )?;

        conn.execute(
            "INSERT INTO gpkgext_relations \
             (base_table_name, base_primary_column, related_table_name, \
              related_primary_column, relation_name, mapping_table_name) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                relation.base_table_name,
                relation.base_primary_column.as_deref().unwrap_or("id"),
                relation.related_table_name,
                relation.related_primary_column.as_deref().unwrap_or("id"),
                relation.relation_name.as_string(),
                relation.mapping_table_name,
            ],
        )?;
        let id = conn.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Map `base_id` to `related_id` in a relationship's mapping table.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchTable`] if the mapping table is absent.
    pub fn add_mapping(&self, relation: &Relation, base_id: i64, related_id: i64) -> Result<()> {
        let conn = self.connection();
        if !table_exists(conn, &relation.mapping_table_name)? {
            return Err(Error::NoSuchTable {
                table_name: relation.mapping_table_name.clone(),
            });
        }
        let sql = format!(
            "INSERT INTO {} (base_id, related_id) VALUES (?1, ?2)",
            quote(&relation.mapping_table_name).map_err(Error::Core)?
        );
        conn.execute(&sql, [base_id, related_id])?;
        Ok(())
    }
}
