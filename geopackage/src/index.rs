//! RTree spatial-index lifecycle on a [`Layer`]: [`Layer::create_spatial_index`],
//! [`Layer::drop_spatial_index`], and [`Layer::repair_spatial_index`].
//!
//! The normative SQL — the `rtree_<table>_<column>` virtual table, the
//! GeoPackage 1.4 trigger set, and the population statement — is emitted by
//! [`geopackage_core::triggers`] (spec Annex F.3, reproduced verbatim there).
//! This module drives it against the live connection and maintains the
//! `gpkg_extensions` registration row (spec Annex F.3 requirements 75/76).
//!
//! Design decision D7: a new index always gets the 1.4 trigger set
//! (`update5`/`update6`/`update7`), which is UPSERT-safe. Older generations are
//! never repaired automatically — [`Layer::repair_spatial_index`] is the sole,
//! explicitly user-invoked path that rewrites an existing trigger set.
//!
//! The D8 bulk-load path (build the rtree in a scratch database and copy its
//! shadow tables) is a later slice; population here is a single
//! `INSERT INTO rtree SELECT` over the existing rows, using the registered
//! `ST_*` functions and skipping empty/NULL geometries exactly as the triggers
//! do.

use geopackage_core::ddl;
use geopackage_core::ident::quote;
use geopackage_core::triggers::{self, TriggerGeneration};
use rusqlite::Connection;

use crate::{Error, GeometryColumn, Layer, Result, table_exists};

impl Layer<'_> {
    /// Build an RTree spatial index over this feature layer's geometry column.
    ///
    /// Creates the `rtree_<table>_<column>` virtual table, installs the
    /// GeoPackage 1.4 trigger set (design decision D7), populates the index from
    /// the existing rows (skipping NULL and empty geometries), and registers the
    /// `gpkg_rtree_index` extension in `gpkg_extensions` (creating that table on
    /// first use). The whole operation is one transaction.
    ///
    /// Population uses a single `INSERT INTO rtree SELECT` driven by the
    /// registered `ST_*` functions. The bulk-load path (design decision D8) that
    /// replaces this for large tables is a later slice.
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] on an attribute layer (or a feature table
    ///   with no `gpkg_geometry_columns` row).
    /// - [`Error::NoPrimaryKey`] if the table has no single-column primary key
    ///   (the index keys its rows on that column).
    /// - [`Error::SpatialIndexExists`] if the `rtree_<table>_<column>` virtual
    ///   table already exists.
    pub fn create_spatial_index(&self) -> Result<()> {
        let geom = self.require_geometry_column()?;
        let pk = self.require_primary_key()?;
        let conn = self.gpkg().connection();
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);
        if table_exists(conn, &rtree)? {
            return Err(Error::SpatialIndexExists {
                table_name: self.table_name().to_owned(),
                column_name: geom.column_name.clone(),
            });
        }

        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(&triggers::create_rtree_table_sql(
            self.table_name(),
            &geom.column_name,
        )?)?;
        for sql in triggers::create_triggers_sql(self.table_name(), &geom.column_name, pk)? {
            tx.execute_batch(&sql)?;
        }
        tx.execute_batch(&triggers::populate_rtree_sql(
            self.table_name(),
            &geom.column_name,
            pk,
        )?)?;
        register_extension_row(&tx, self.table_name(), &geom.column_name)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove this layer's RTree spatial index: its triggers, the
    /// `rtree_<table>_<column>` virtual table (and its shadow tables), and the
    /// `gpkg_extensions` registration row. The `gpkg_extensions` table itself is
    /// left in place.
    ///
    /// Idempotent: a layer with no index is left unchanged and returns `Ok`.
    /// Removal covers any trigger generation, so it also cleans up a legacy or
    /// mixed set.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] on a layer with no geometry column.
    pub fn drop_spatial_index(&self) -> Result<()> {
        let geom = self.require_geometry_column()?;
        let conn = self.gpkg().connection();
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);

        let tx = conn.unchecked_transaction()?;
        drop_all_rtree_triggers(&tx, self.table_name(), &geom.column_name)?;
        tx.execute_batch(&format!("DROP TABLE IF EXISTS {}", quote(&rtree)?))?;
        if table_exists(&tx, "gpkg_extensions")? {
            tx.execute(
                "DELETE FROM gpkg_extensions \
                 WHERE table_name = ?1 AND column_name = ?2 AND extension_name = ?3",
                rusqlite::params![
                    self.table_name(),
                    geom.column_name,
                    triggers::EXTENSION_NAME
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Upgrade a legacy or inconsistent RTree trigger set to the GeoPackage 1.4
    /// set and rebuild the index content (design decision D7).
    ///
    /// The pre-1.4 `update1` trigger corrupts an index under `UPSERT`; 1.4
    /// renamed the fixed triggers so the repaired state is detectable by name.
    /// This is **never** invoked automatically — reading a file never mutates
    /// it.
    ///
    /// Behaviour by trigger generation ([`Self::has_spatial_index`] classifies
    /// the same way):
    ///
    /// - [`TriggerGeneration::V1_4`]: already current, a no-op.
    /// - [`TriggerGeneration::PreV1_4`] or [`TriggerGeneration::Mixed`]: drop
    ///   every RTree trigger, install the 1.4 set, and rebuild the index from
    ///   the current rows.
    /// - [`TriggerGeneration::None`]: [`Error::NoSpatialIndex`] — there is
    ///   nothing to repair; use [`Self::create_spatial_index`].
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] on a layer with no geometry column.
    /// - [`Error::NoPrimaryKey`] if the table has no single-column primary key.
    /// - [`Error::NoSpatialIndex`] if no RTree triggers are present.
    pub fn repair_spatial_index(&self) -> Result<()> {
        let geom = self.require_geometry_column()?;
        let pk = self.require_primary_key()?;
        let generation = self.classify_rtree_triggers(&geom.column_name)?;
        match generation {
            TriggerGeneration::V1_4 => Ok(()),
            TriggerGeneration::None => Err(Error::NoSpatialIndex {
                table_name: self.table_name().to_owned(),
                column_name: geom.column_name.clone(),
            }),
            TriggerGeneration::PreV1_4 | TriggerGeneration::Mixed => {
                let conn = self.gpkg().connection();
                let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);

                let tx = conn.unchecked_transaction()?;
                drop_all_rtree_triggers(&tx, self.table_name(), &geom.column_name)?;
                for sql in triggers::create_triggers_sql(self.table_name(), &geom.column_name, pk)?
                {
                    tx.execute_batch(&sql)?;
                }
                // Rebuild the index content. The virtual table is normally
                // present alongside legacy triggers; create it if a corrupt
                // file lost it, so the repopulation below cannot fail on a
                // missing table.
                if !table_exists(&tx, &rtree)? {
                    tx.execute_batch(&triggers::create_rtree_table_sql(
                        self.table_name(),
                        &geom.column_name,
                    )?)?;
                } else {
                    tx.execute_batch(&format!("DELETE FROM {}", quote(&rtree)?))?;
                }
                tx.execute_batch(&triggers::populate_rtree_sql(
                    self.table_name(),
                    &geom.column_name,
                    pk,
                )?)?;
                tx.commit()?;
                Ok(())
            }
        }
    }

    /// This layer's geometry column, or [`Error::NoGeometryColumn`] when it has
    /// none (an attribute layer, or a feature table with no
    /// `gpkg_geometry_columns` row).
    fn require_geometry_column(&self) -> Result<&GeometryColumn> {
        self.geometry_column()
            .ok_or_else(|| Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            })
    }

    /// This layer's single-column primary key, or [`Error::NoPrimaryKey`].
    fn require_primary_key(&self) -> Result<&str> {
        self.primary_key_column()
            .ok_or_else(|| Error::NoPrimaryKey {
                table_name: self.table_name().to_owned(),
            })
    }
}

/// Register the `gpkg_rtree_index` extension for `table`/`column`, creating
/// `gpkg_extensions` on first use. The `extension_name`, `definition`, and
/// `scope` values are the spec-prescribed constants from
/// [`geopackage_core::triggers`] (Annex F.3, requirements 75/76).
fn register_extension_row(conn: &Connection, table: &str, column: &str) -> Result<()> {
    if !table_exists(conn, "gpkg_extensions")? {
        conn.execute_batch(ddl::CREATE_GPKG_EXTENSIONS)?;
    }
    conn.execute(
        "INSERT INTO gpkg_extensions \
         (table_name, column_name, extension_name, definition, scope) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            table,
            column,
            triggers::EXTENSION_NAME,
            triggers::EXTENSION_DEFINITION,
            triggers::EXTENSION_SCOPE,
        ],
    )?;
    Ok(())
}

/// Drop every RTree trigger for `table`/`column`, of any generation, by name.
///
/// Triggers fire on the user table, so their `sqlite_master.tbl_name` is
/// `table`; the ones belonging to this index share the `rtree_<table>_<column>_`
/// name prefix (the same prefix rule [`triggers::classify_triggers`] uses).
fn drop_all_rtree_triggers(conn: &Connection, table: &str, column: &str) -> Result<()> {
    let prefix = format!("{}_", triggers::rtree_table_name(table, column));
    let names: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1")?;
        stmt.query_map([table], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for name in names.iter().filter(|n| n.starts_with(&prefix)) {
        conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {}", quote(name)?))?;
    }
    Ok(())
}
