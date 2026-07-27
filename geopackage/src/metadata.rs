//! The `gpkg_metadata` extension's two tables: `gpkg_metadata`, which holds
//! documents, and `gpkg_metadata_reference`, which attaches them to a file's
//! contents.
//!
//! Documents are stored and returned as written. What standard a document
//! follows is what `md_standard_uri` and `mime_type` say, and this crate reads
//! neither, so no XML parser is pulled in and no profile is interpreted. See
//! [`geopackage_core::metadata`] for the model.
//!
//! # References form a graph, and this module hands back its edges
//!
//! A reference may name a parent record through `md_parent_id`, so the
//! attachments form a directed graph rather than a list. [`GeoPackage::
//! metadata_references`] returns the edges as stored. Walking upwards is
//! [`GeoPackage::metadata_ancestors`], which is a separate call because it can
//! fail in ways enumeration cannot: Requirement 102 forbids only a record being
//! its own parent, so a longer cycle is a file this crate has to survive rather
//! than a case it can rule out. That walk is bounded and reports a cycle as a
//! typed error, which is not a cost every enumeration should pay.

use geopackage_core::datetime::DateTime;
use geopackage_core::ddl;
use geopackage_core::metadata::{
    EXTENSION_DEFINITION, EXTENSION_NAME, EXTENSION_SCOPE, MetadataRecord, MetadataReference,
    MetadataScope, ReferenceScope,
};
use rusqlite::{Connection, OptionalExtension};

use crate::{Error, GeoPackage, Result, table_exists};

const METADATA_TABLE: &str = "gpkg_metadata";
const REFERENCE_TABLE: &str = "gpkg_metadata_reference";

/// What a metadata reference is attached to, as a caller states it.
///
/// The spec splits this across `reference_scope`, `table_name`, `column_name`
/// and `row_id_value`, with Requirements 97 to 99 deciding which of the three
/// are NULL for a given scope. Stating the target as one value makes an
/// unrepresentable combination unrepresentable rather than merely invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataTarget {
    /// The GeoPackage as a whole.
    GeoPackage,
    /// One table or view.
    Table {
        /// The table's name.
        table_name: String,
    },
    /// One column of one table.
    Column {
        /// The table's name.
        table_name: String,
        /// The column's name.
        column_name: String,
    },
    /// One row of one table, by `ROWID`.
    Row {
        /// The table's name.
        table_name: String,
        /// The row's `ROWID`.
        row_id: i64,
    },
    /// One cell: a row and a column.
    Cell {
        /// The table's name.
        table_name: String,
        /// The column's name.
        column_name: String,
        /// The row's `ROWID`.
        row_id: i64,
    },
}

impl MetadataTarget {
    /// The `reference_scope` this target is written as.
    pub fn scope(&self) -> ReferenceScope {
        match self {
            Self::GeoPackage => ReferenceScope::GeoPackage,
            Self::Table { .. } => ReferenceScope::Table,
            Self::Column { .. } => ReferenceScope::Column,
            Self::Row { .. } => ReferenceScope::Row,
            Self::Cell { .. } => ReferenceScope::RowCol,
        }
    }

    /// The table this target names, or `None` for the whole GeoPackage.
    pub fn table_name(&self) -> Option<&str> {
        match self {
            Self::GeoPackage => None,
            Self::Table { table_name }
            | Self::Column { table_name, .. }
            | Self::Row { table_name, .. }
            | Self::Cell { table_name, .. } => Some(table_name),
        }
    }

    fn column_name(&self) -> Option<&str> {
        match self {
            Self::Column { column_name, .. } | Self::Cell { column_name, .. } => Some(column_name),
            _ => None,
        }
    }

    fn row_id(&self) -> Option<i64> {
        match self {
            Self::Row { row_id, .. } | Self::Cell { row_id, .. } => Some(*row_id),
            _ => None,
        }
    }

    /// Rebuild a target from the four stored columns, or `None` when they do
    /// not satisfy Requirements 97 to 99 for their scope.
    fn from_columns(
        scope: ReferenceScope,
        table_name: Option<String>,
        column_name: Option<String>,
        row_id: Option<i64>,
    ) -> Option<Self> {
        Some(match (scope, table_name, column_name, row_id) {
            (ReferenceScope::GeoPackage, None, None, None) => Self::GeoPackage,
            (ReferenceScope::Table, Some(table_name), None, None) => Self::Table { table_name },
            (ReferenceScope::Column, Some(table_name), Some(column_name), None) => Self::Column {
                table_name,
                column_name,
            },
            (ReferenceScope::Row, Some(table_name), None, Some(row_id)) => {
                Self::Row { table_name, row_id }
            }
            (ReferenceScope::RowCol, Some(table_name), Some(column_name), Some(row_id)) => {
                Self::Cell {
                    table_name,
                    column_name,
                    row_id,
                }
            }
            _ => return None,
        })
    }
}

/// A metadata document to add, before it has an `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMetadata {
    /// What the document describes.
    pub scope: MetadataScope,
    /// URI of the metadata standard the document follows.
    pub standard_uri: String,
    /// MIME type of the document. `text/xml` is the table's default.
    pub mime_type: String,
    /// The document itself, stored as given.
    pub metadata: String,
}

impl NewMetadata {
    /// A document with the table's default MIME type, `text/xml`.
    pub fn new(
        scope: MetadataScope,
        standard_uri: impl Into<String>,
        metadata: impl Into<String>,
    ) -> Self {
        Self {
            scope,
            standard_uri: standard_uri.into(),
            mime_type: "text/xml".to_owned(),
            metadata: metadata.into(),
        }
    }

    /// Override the MIME type.
    #[must_use]
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = mime_type.into();
        self
    }
}

impl GeoPackage {
    /// Every `gpkg_metadata` record, by `id`.
    ///
    /// An empty vector for a file with no metadata table, which is the common
    /// case rather than an error.
    ///
    /// # Errors
    ///
    /// [`Error`] if the tables cannot be read.
    pub fn metadata(&self) -> Result<Vec<MetadataRecord>> {
        let conn = self.connection();
        if !table_exists(conn, METADATA_TABLE)? {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT id, md_scope, md_standard_uri, mime_type, metadata \
             FROM gpkg_metadata ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let scope: String = row.get(1)?;
                Ok(MetadataRecord {
                    id: row.get(0)?,
                    scope: MetadataScope::parse(&scope),
                    standard_uri: row.get(2)?,
                    mime_type: row.get(3)?,
                    metadata: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One `gpkg_metadata` record by `id`, or `None`.
    ///
    /// # Errors
    ///
    /// [`Error`] if the table cannot be read.
    pub fn metadata_record(&self, id: i64) -> Result<Option<MetadataRecord>> {
        let conn = self.connection();
        if !table_exists(conn, METADATA_TABLE)? {
            return Ok(None);
        }
        let record = conn
            .query_row(
                "SELECT id, md_scope, md_standard_uri, mime_type, metadata \
                 FROM gpkg_metadata WHERE id = ?1",
                [id],
                |row| {
                    let scope: String = row.get(1)?;
                    Ok(MetadataRecord {
                        id: row.get(0)?,
                        scope: MetadataScope::parse(&scope),
                        standard_uri: row.get(2)?,
                        mime_type: row.get(3)?,
                        metadata: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    /// Add a metadata document, returning its assigned `id`.
    ///
    /// Creates both extension tables and registers the extension on first use.
    ///
    /// # Errors
    ///
    /// [`Error`] if the tables cannot be created or the row cannot be written.
    pub fn add_metadata(&self, metadata: &NewMetadata) -> Result<i64> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        ensure_tables(&tx)?;
        tx.execute(
            "INSERT INTO gpkg_metadata (md_scope, md_standard_uri, mime_type, metadata) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                metadata.scope.as_str(),
                metadata.standard_uri,
                metadata.mime_type,
                metadata.metadata,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Attach a metadata record to a target.
    ///
    /// `parent_id` names a record this one refines. Requirement 102 forbids it
    /// equalling `md_file_id`, which is rejected here rather than written.
    ///
    /// The timestamp is written in the spec's DATETIME form through
    /// [`geopackage_core::datetime`], the same path every other DATETIME in
    /// this crate takes.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchMetadata`] if `md_file_id` or `parent_id` is not a
    ///   `gpkg_metadata` row.
    /// - [`Error::SelfParentedMetadata`] if `parent_id` equals `md_file_id`.
    /// - [`Error::NoSuchTable`] if the target names a table that is not in
    ///   `gpkg_contents` (Requirement 97).
    pub fn add_metadata_reference(
        &self,
        md_file_id: i64,
        target: &MetadataTarget,
        timestamp: DateTime,
        parent_id: Option<i64>,
    ) -> Result<()> {
        if parent_id == Some(md_file_id) {
            return Err(Error::SelfParentedMetadata { md_file_id });
        }
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        ensure_tables(&tx)?;

        for id in std::iter::once(md_file_id).chain(parent_id) {
            let exists: Option<i64> = tx
                .query_row("SELECT id FROM gpkg_metadata WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            if exists.is_none() {
                return Err(Error::NoSuchMetadata { id });
            }
        }

        // Requirement 97: every scope but `geopackage` names a table that is in
        // gpkg_contents.
        if let Some(table_name) = target.table_name() {
            let known: Option<String> = tx
                .query_row(
                    "SELECT table_name FROM gpkg_contents WHERE table_name = ?1",
                    [table_name],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_none() {
                return Err(Error::NoSuchTable {
                    table_name: table_name.to_owned(),
                });
            }
        }

        tx.execute(
            "INSERT INTO gpkg_metadata_reference \
             (reference_scope, table_name, column_name, row_id_value, timestamp, \
              md_file_id, md_parent_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                target.scope().as_str(),
                target.table_name(),
                target.column_name(),
                target.row_id(),
                timestamp.to_string(),
                md_file_id,
                parent_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every `gpkg_metadata_reference` row, as stored.
    ///
    /// The edges of the reference graph, not a traversal of it: see the module
    /// documentation, and [`GeoPackage::metadata_ancestors`] for the walk.
    ///
    /// # Errors
    ///
    /// [`Error`] if the table cannot be read, or if a row's `reference_scope`
    /// is not one of the five Requirement 96 allows.
    pub fn metadata_references(&self) -> Result<Vec<MetadataReference>> {
        let conn = self.connection();
        if !table_exists(conn, REFERENCE_TABLE)? {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT reference_scope, table_name, column_name, row_id_value, \
                    timestamp, md_file_id, md_parent_id \
             FROM gpkg_metadata_reference",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let scope: String = row.get(0)?;
                Ok((
                    scope,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        rows.into_iter()
            .map(
                |(scope, table_name, column_name, row_id_value, timestamp, md_file_id, parent)| {
                    let scope = ReferenceScope::parse(&scope)
                        .ok_or(Error::UnknownReferenceScope { scope })?;
                    Ok(MetadataReference {
                        scope,
                        table_name,
                        column_name,
                        row_id_value,
                        timestamp,
                        md_file_id,
                        md_parent_id: parent,
                    })
                },
            )
            .collect()
    }

    /// The references attached to `target`.
    ///
    /// # Errors
    ///
    /// As [`GeoPackage::metadata_references`].
    pub fn metadata_for(&self, target: &MetadataTarget) -> Result<Vec<MetadataReference>> {
        let wanted = target.scope();
        let references = self.metadata_references()?;
        Ok(references
            .into_iter()
            .filter(|reference| {
                reference.scope == wanted
                    && MetadataTarget::from_columns(
                        reference.scope,
                        reference.table_name.clone(),
                        reference.column_name.clone(),
                        reference.row_id_value,
                    )
                    .as_ref()
                        == Some(target)
            })
            .collect())
    }

    /// Walk `md_parent_id` upwards from `id`, nearest parent first.
    ///
    /// The returned records do not include `id` itself. The walk is the reason
    /// enumeration hands back edges rather than a tree: `md_parent_id` is a
    /// graph, and Requirement 102 rules out only the one-step cycle, so a
    /// longer one has to be survived rather than assumed away.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchMetadata`] if `id` is not a `gpkg_metadata` row.
    /// - [`Error::MetadataCycle`] if the chain revisits a record.
    pub fn metadata_ancestors(&self, id: i64) -> Result<Vec<MetadataRecord>> {
        if self.metadata_record(id)?.is_none() {
            return Err(Error::NoSuchMetadata { id });
        }
        let conn = self.connection();
        let mut seen = vec![id];
        let mut ancestors = Vec::new();
        let mut current = id;

        while let Some(parent) = conn
            .query_row(
                "SELECT md_parent_id FROM gpkg_metadata_reference \
                 WHERE md_file_id = ?1 AND md_parent_id IS NOT NULL LIMIT 1",
                [current],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
        {
            if seen.contains(&parent) {
                return Err(Error::MetadataCycle { id: parent });
            }
            let Some(record) = self.metadata_record(parent)? else {
                return Err(Error::NoSuchMetadata { id: parent });
            };
            seen.push(parent);
            ancestors.push(record);
            current = parent;
        }
        Ok(ancestors)
    }
}

/// Create both tables and register the extension, once.
///
/// Both are registered even when only one is about to gain rows: the extension
/// is the pair, and GDAL writes a row for each, which is what Annex F.8's test
/// for the extension expects to find.
fn ensure_tables(conn: &Connection) -> Result<()> {
    if table_exists(conn, METADATA_TABLE)? {
        return Ok(());
    }
    conn.execute_batch(ddl::CREATE_GPKG_METADATA)?;
    conn.execute_batch(ddl::CREATE_GPKG_METADATA_REFERENCE)?;
    for table in [METADATA_TABLE, REFERENCE_TABLE] {
        crate::extensions::register(
            conn,
            Some(table),
            None,
            EXTENSION_NAME,
            EXTENSION_DEFINITION,
            EXTENSION_SCOPE,
        )?;
    }
    Ok(())
}
