//! The `gpkg_schema` extension's two tables: `gpkg_data_columns`, which
//! describes a column, and `gpkg_data_column_constraints`, which stores the
//! constraints those descriptions point at.
//!
//! Both are read here and written here. A constraint is assembled from the
//! rows sharing its name rather than returned row by row, since an `enum`
//! occupies one row per member while a `range` or `glob` occupies exactly one
//! (Requirement 109). The descriptions themselves are attached to
//! [`crate::Column`] by [`crate::GeoPackage::table_schema`], so a caller
//! reading a layer's schema sees them without asking twice.

use geopackage_core::ddl;
use geopackage_core::schema::{
    ColumnConstraint, ConstraintKind, DataColumn, EXTENSION_DEFINITION, EXTENSION_NAME,
    EXTENSION_SCOPE,
};
use rusqlite::Connection;

use crate::transaction::WriteTransaction;
use crate::{Error, GeoPackage, Result, table_exists};

const DATA_COLUMNS_TABLE: &str = "gpkg_data_columns";
const CONSTRAINTS_TABLE: &str = "gpkg_data_column_constraints";

/// Which spelling of the inclusivity columns a file uses.
///
/// GeoPackage 1.0 named them `minIsInclusive` and `maxIsInclusive`; 1.1
/// corrected that to `min_is_inclusive` and `max_is_inclusive`, and the spec
/// warns that files written under the old names are still about. SQLite column
/// names are case-insensitive, so the difference is the underscores.
#[derive(Debug, Clone, Copy)]
struct InclusiveColumns {
    min: &'static str,
    max: &'static str,
}

impl InclusiveColumns {
    fn read(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({CONSTRAINTS_TABLE})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row
                .get::<_, String>(1)?
                .eq_ignore_ascii_case("minisinclusive")
            {
                return Ok(Self {
                    min: "minIsInclusive",
                    max: "maxIsInclusive",
                });
            }
        }
        Ok(Self {
            min: "min_is_inclusive",
            max: "max_is_inclusive",
        })
    }
}

impl GeoPackage {
    /// Returns `true` if written values are checked against the constraints
    /// their columns declare. See
    /// [`OpenOptions::enforce_column_constraints`](crate::OpenOptions::enforce_column_constraints).
    pub fn enforces_column_constraints(&self) -> bool {
        self.enforce_column_constraints
    }

    /// Returns the `gpkg_data_columns` rows describing one table's columns,
    /// in column order as the file stores them.
    ///
    /// Empty for a file without the `gpkg_schema` extension, which is the
    /// common case rather than an error. Table names are compared
    /// case-insensitively, as elsewhere in the catalogue.
    pub fn data_columns(&self, table_name: &str) -> Result<Vec<DataColumn>> {
        let conn = self.connection();
        if !table_exists(conn, DATA_COLUMNS_TABLE)? {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT column_name, name, title, description, mime_type, constraint_name \
             FROM {DATA_COLUMNS_TABLE} WHERE lower(table_name) = lower(?1) ORDER BY column_name"
        ))?;
        let rows = stmt.query_map([table_name], |r| {
            Ok(DataColumn {
                column_name: r.get(0)?,
                name: r.get(1)?,
                title: r.get(2)?,
                description: r.get(3)?,
                mime_type: r.get(4)?,
                constraint_name: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Returns the constraint of the given name, assembled from every row
    /// that uses it.
    ///
    /// `None` for a file without the extension, or a name no row uses.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidColumnConstraint`] for rows that declare a constraint
    /// the spec's own rules rule out: a `constraint_type` outside the three of
    /// Requirement 108, or a `range` without the bounds Requirement 111 makes
    /// mandatory. Reading such a file is not an error until something asks
    /// what the constraint allows, which such a row cannot answer.
    pub fn column_constraint(&self, name: &str) -> Result<Option<ColumnConstraint>> {
        let conn = self.connection();
        if !table_exists(conn, CONSTRAINTS_TABLE)? {
            return Ok(None);
        }
        let inclusive = InclusiveColumns::read(conn)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT constraint_type, value, min, {}, max, {}, description \
             FROM {CONSTRAINTS_TABLE} WHERE constraint_name = ?1 \
             ORDER BY rowid",
            inclusive.min, inclusive.max
        ))?;
        let rows: Vec<ConstraintRow> = stmt
            .query_map([name], |r| {
                Ok(ConstraintRow {
                    constraint_type: r.get(0)?,
                    value: r.get(1)?,
                    min: r.get(2)?,
                    min_is_inclusive: r.get(3)?,
                    max: r.get(4)?,
                    max_is_inclusive: r.get(5)?,
                    description: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        assemble(name, rows)
    }

    /// Returns every constraint in the file, by name.
    pub fn column_constraints(&self) -> Result<Vec<ColumnConstraint>> {
        let conn = self.connection();
        if !table_exists(conn, CONSTRAINTS_TABLE)? {
            return Ok(Vec::new());
        }
        let names: Vec<String> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT constraint_name FROM {CONSTRAINTS_TABLE} ORDER BY constraint_name"
            ))?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        names
            .into_iter()
            .filter_map(|name| self.column_constraint(&name).transpose())
            .collect()
    }

    /// Describes a column, replacing any description it already has.
    ///
    /// Creates the extension's tables and registers `gpkg_schema` on first
    /// use. The primary key is the table and column pair, so this is an upsert
    /// rather than an insert: a column has one description or none.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchColumn`] if `table_name` has no such column
    /// (Requirement 105), and [`Error::NoSuchTable`] if the table itself is
    /// absent.
    pub fn set_data_column(&self, table_name: &str, data_column: &DataColumn) -> Result<()> {
        let schema = self.table_schema(table_name)?;
        if schema.column(&data_column.column_name).is_none() {
            return Err(Error::NoSuchColumn {
                table_name: table_name.to_owned(),
                column_name: data_column.column_name.clone(),
            });
        }
        let conn = self.connection();
        let tx = WriteTransaction::begin(conn)?;
        ensure_tables(conn)?;
        conn.execute(
            &format!(
                "INSERT INTO {DATA_COLUMNS_TABLE} \
                 (table_name, column_name, name, title, description, mime_type, constraint_name) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (table_name, column_name) DO UPDATE SET \
                 name = excluded.name, title = excluded.title, \
                 description = excluded.description, mime_type = excluded.mime_type, \
                 constraint_name = excluded.constraint_name"
            ),
            rusqlite::params![
                table_name,
                data_column.column_name,
                data_column.name,
                data_column.title,
                data_column.description,
                data_column.mime_type,
                data_column.constraint_name,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Adds a constraint, replacing any of the same name.
    ///
    /// Creates the extension's tables and registers `gpkg_schema` on first
    /// use. An `enum` becomes one row per member and a `range` or `glob` one
    /// row, which is the shape Requirement 109 describes.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidColumnConstraint`] for a range whose `min` is not below
    /// its `max` (Requirement 111), or an enum with no members, which no row
    /// could express.
    pub fn add_column_constraint(&self, constraint: &ColumnConstraint) -> Result<()> {
        let conn = self.connection();
        let tx = WriteTransaction::begin(conn)?;
        ensure_tables(conn)?;
        conn.execute(
            &format!("DELETE FROM {CONSTRAINTS_TABLE} WHERE constraint_name = ?1"),
            [&constraint.name],
        )?;
        let inclusive = InclusiveColumns::read(conn)?;
        let insert = format!(
            "INSERT INTO {CONSTRAINTS_TABLE} \
             (constraint_name, constraint_type, value, min, {}, max, {}, description) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            inclusive.min, inclusive.max
        );
        match &constraint.kind {
            ConstraintKind::Range {
                min,
                min_is_inclusive,
                max,
                max_is_inclusive,
            } => {
                if min.partial_cmp(max) != Some(std::cmp::Ordering::Less) {
                    return Err(Error::InvalidColumnConstraint {
                        constraint_name: constraint.name.clone(),
                        reason: "a range needs min below max (Requirement 111)",
                    });
                }
                conn.execute(
                    &insert,
                    rusqlite::params![
                        constraint.name,
                        "range",
                        None::<String>,
                        min,
                        min_is_inclusive,
                        max,
                        max_is_inclusive,
                        constraint.description,
                    ],
                )?;
            }
            ConstraintKind::Enum(members) => {
                if members.is_empty() {
                    return Err(Error::InvalidColumnConstraint {
                        constraint_name: constraint.name.clone(),
                        reason: "an enum with no members cannot be written as rows",
                    });
                }
                for member in members {
                    conn.execute(
                        &insert,
                        rusqlite::params![
                            constraint.name,
                            "enum",
                            member,
                            None::<f64>,
                            None::<bool>,
                            None::<f64>,
                            None::<bool>,
                            constraint.description,
                        ],
                    )?;
                }
            }
            ConstraintKind::Glob(pattern) => {
                conn.execute(
                    &insert,
                    rusqlite::params![
                        constraint.name,
                        "glob",
                        pattern,
                        None::<f64>,
                        None::<bool>,
                        None::<f64>,
                        None::<bool>,
                        constraint.description,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// One `gpkg_data_column_constraints` row, before the rows of a name are
/// assembled into a constraint.
struct ConstraintRow {
    constraint_type: String,
    value: Option<String>,
    min: Option<f64>,
    min_is_inclusive: Option<bool>,
    max: Option<f64>,
    max_is_inclusive: Option<bool>,
    description: Option<String>,
}

/// Turn the rows sharing a name into one constraint.
fn assemble(name: &str, rows: Vec<ConstraintRow>) -> Result<Option<ColumnConstraint>> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let invalid = |reason: &'static str| Error::InvalidColumnConstraint {
        constraint_name: name.to_owned(),
        reason,
    };
    let kind = match first.constraint_type.as_str() {
        "range" => ConstraintKind::Range {
            min: first.min.ok_or_else(|| invalid("a range needs a min"))?,
            // Requirement 112 makes the flags 0 or 1, but a NULL is common
            // enough in the wild to be worth a reading rather than an error:
            // an inclusive bound is the ordinary intent.
            min_is_inclusive: first.min_is_inclusive.unwrap_or(true),
            max: first.max.ok_or_else(|| invalid("a range needs a max"))?,
            max_is_inclusive: first.max_is_inclusive.unwrap_or(true),
        },
        "glob" => ConstraintKind::Glob(
            first
                .value
                .clone()
                .ok_or_else(|| invalid("a glob needs a pattern in its value column"))?,
        ),
        "enum" => ConstraintKind::Enum(
            rows.iter()
                .filter(|row| row.constraint_type == "enum")
                .map(|row| {
                    row.value
                        .clone()
                        .ok_or_else(|| invalid("an enum member needs a value"))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        _ => {
            return Err(invalid(
                "constraint_type is not one of range, enum or glob (Requirement 108)",
            ));
        }
    };
    Ok(Some(ColumnConstraint {
        name: name.to_owned(),
        kind,
        description: rows.iter().find_map(|row| row.description.clone()),
    }))
}

/// Creates both tables and registers the extension, once.
///
/// Requirement 141 asks for a row per table, so both are registered together
/// even though only one of them may be about to gain rows: the tables are
/// created together, and a `gpkg_data_columns` with no constraints table to
/// point at would be a file half in the extension.
fn ensure_tables(conn: &Connection) -> Result<()> {
    if table_exists(conn, DATA_COLUMNS_TABLE)? {
        return Ok(());
    }
    conn.execute_batch(ddl::CREATE_GPKG_DATA_COLUMNS)?;
    conn.execute_batch(ddl::CREATE_GPKG_DATA_COLUMN_CONSTRAINTS)?;
    for table in [DATA_COLUMNS_TABLE, CONSTRAINTS_TABLE] {
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
