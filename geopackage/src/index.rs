//! RTree spatial-index lifecycle on a [`Layer`]: [`Layer::create_spatial_index`],
//! [`Layer::drop_spatial_index`], [`Layer::repair_spatial_index`],
//! [`Layer::audit_spatial_index`] and [`Layer::rebuild_spatial_index`].
//!
//! Two questions can be asked of an index and they have different prices.
//! [`Layer::spatial_index_status`] is structural: does the virtual table exist,
//! and which generation of trigger set maintains it. It is cheap, it is what
//! [`Layer::repair_spatial_index`] acts on, and it cannot tell whether the
//! entries agree with the geometries. [`Layer::audit_spatial_index`] answers
//! that second question by reading every geometry in the layer, and
//! [`Layer::rebuild_spatial_index`] is its remedy, doing unconditionally what
//! the repair does only when the structure is wrong.
//!
//! Of these, only the audit and the status never write; see the crate-level
//! documentation for what each call does on a read-only connection.
//!
//! The normative SQL (the `rtree_<table>_<column>` virtual table, the
//! GeoPackage 1.4 trigger set, and the population statement) is emitted by
//! [`geopackage_core::triggers`] (spec Annex F.3, reproduced verbatim there).
//! This module drives it against the live connection and maintains the
//! `gpkg_extensions` registration row (spec Annex F.3 requirements 75/76).
//!
//! A new index always gets the 1.4 trigger set (`update5`/`update6`/`update7`),
//! which is UPSERT-safe. Older generations are never repaired automatically:
//! [`Layer::repair_spatial_index`] is the sole, explicitly user-invoked path
//! that rewrites an existing trigger set.
//!
//! That is a judgement about indexes rather than a rule that reading never
//! writes, which is not one this crate keeps: [`Layer::extent`] records an
//! extent it had to measure. The difference is what the two cost and what
//! their absence does. Rebuilding an index reads every geometry in the layer
//! and rewrites the tree, which is not a bill to hand someone who asked a
//! question; and an index that is stale or of an older generation is not
//! silently believed, because [`Layer::has_spatial_index`] reports it as
//! unusable and queries fall back to a correct scan. An extent is neither:
//! measuring it is comparatively cheap, and a wrong one is believed
//! indefinitely by every reader.
//!
//! Population takes one of two paths. Below the [`BulkIndexOptions`] threshold
//! it is a single `INSERT INTO rtree SELECT` over the existing rows, using the
//! registered `ST_*` functions and skipping empty/NULL geometries exactly as
//! the triggers do. At or above the threshold it is the bulk shadow-table build
//! in [`crate::bulk`]: accumulate the envelopes, build the tree in memory, and
//! write the shadow tables directly, gated with automatic fallback to the
//! triggered path.

use geopackage_core::ident::quote;
use geopackage_core::triggers::{self, TriggerGeneration};
use rusqlite::Connection;

use crate::bulk::{self, BuildPath, BulkIndexOptions, TestFault};
use crate::{Error, GeometryColumn, Layer, Result, table_exists};

/// The health of a layer's RTree spatial index, from
/// [`Layer::spatial_index_status`].
///
/// Derived from two signals: whether the `rtree_<table>_<column>` virtual table
/// exists, and which generation of trigger set (if any) maintains it
/// ([`TriggerGeneration`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpatialIndexStatus {
    /// No spatial index: neither the virtual table nor any RTree trigger is
    /// present. Build one with [`Layer::create_spatial_index`].
    Absent,
    /// A complete, current index: the virtual table exists and carries the
    /// GeoPackage 1.4 trigger set. [`Layer::features_in`] uses it.
    Current,
    /// A present index maintained by a legacy (pre-1.4) or mixed trigger set.
    /// Usable, but the pre-1.4 `update1` trigger corrupts the index under
    /// `UPSERT`; [`Layer::repair_spatial_index`] upgrades it to the 1.4 set.
    Legacy,
    /// A desynchronised index: the virtual table exists but its triggers are
    /// missing (or triggers exist with no table). This crate's own writes
    /// cannot produce it. A bulk [`Layer::write_all`] drops the triggers,
    /// inserts the rows, rebuilds the index and reinstalls the triggers in one
    /// transaction, so an interrupted call rolls back to the state before it.
    /// The state is reachable from a file another tool wrote, or from SQL run
    /// directly against the connection.
    ///
    /// The index is **not** silently trusted: [`Layer::has_spatial_index`]
    /// reports `false` for it, so [`Layer::features_in`] falls back to a correct
    /// full scan. [`Layer::repair_spatial_index`] rebuilds it and reinstalls the
    /// 1.4 triggers.
    Stale,
}

/// What an [`Layer::audit_spatial_index`] found: how the index's contents
/// compare with the geometries they are supposed to describe.
///
/// [`SpatialIndexStatus`] answers a structural question, whether the virtual
/// table and the right triggers are present, and it is cheap. This answers the
/// question that structure cannot: whether the entries actually agree with the
/// data. An index can be [`SpatialIndexStatus::Current`] and still be wrong, in
/// a file where rows were written while the triggers were absent, or that
/// another tool populated incompletely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SpatialIndexAudit {
    /// Rows that ought to be indexed: geometry present and not empty. This is
    /// the same rule the triggers and the bulk build apply.
    pub indexable: usize,
    /// Entries in the RTree.
    pub entries: usize,
    /// Indexable rows with no entry. These are lost by a bounding-box query.
    pub missing: usize,
    /// Entries whose row is gone, or whose geometry is now NULL or empty.
    /// These make a query return a feature id it then cannot read.
    pub extra: usize,
    /// Entries present but not covering their geometry's true envelope. Also
    /// lossy, and not explained by the RTree's `f32` storage, which rounds
    /// outward and so can only ever make an entry too large.
    pub not_covering: usize,
}

impl SpatialIndexAudit {
    /// Whether the index describes exactly the rows it should, covering each.
    ///
    /// False means a bounding-box query can return the wrong answer, and that
    /// [`Layer::rebuild_spatial_index`] is the fix.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.missing == 0 && self.extra == 0 && self.not_covering == 0
    }
}

impl Layer<'_> {
    /// Classify this layer's RTree spatial index (see [`SpatialIndexStatus`]).
    ///
    /// A layer with no geometry column is [`SpatialIndexStatus::Absent`]. This
    /// is the detector for the interrupted-bulk-build case: a
    /// [`SpatialIndexStatus::Stale`] result directs the caller to
    /// [`Self::repair_spatial_index`].
    pub fn spatial_index_status(&self) -> Result<SpatialIndexStatus> {
        let Some(geom) = self.geometry_column() else {
            return Ok(SpatialIndexStatus::Absent);
        };
        let conn = self.gpkg().connection();
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);
        let rtree_exists = table_exists(conn, &rtree)?;
        let generation = self.classify_rtree_triggers(&geom.column_name)?;
        Ok(match (rtree_exists, generation) {
            (false, TriggerGeneration::None) => SpatialIndexStatus::Absent,
            (true, TriggerGeneration::V1_4) => SpatialIndexStatus::Current,
            (true, TriggerGeneration::PreV1_4 | TriggerGeneration::Mixed) => {
                SpatialIndexStatus::Legacy
            }
            // Virtual table without triggers (interrupted bulk build), or
            // triggers without a table: a desynchronised, repairable index.
            (true, TriggerGeneration::None) | (false, _) => SpatialIndexStatus::Stale,
        })
    }
}

impl Layer<'_> {
    /// Build an RTree spatial index over this feature layer's geometry column.
    ///
    /// Creates the `rtree_<table>_<column>` virtual table, installs the
    /// GeoPackage 1.4 trigger set (the UPSERT-safe generation), populates the
    /// index from the existing rows (skipping NULL and empty geometries), and
    /// registers the `gpkg_rtree_index` extension in `gpkg_extensions`
    /// (creating that table on first use). The whole operation is one
    /// transaction.
    ///
    /// Population takes the per-row triggered path (a single
    /// `INSERT INTO rtree SELECT` driven by the registered `ST_*` functions) for
    /// tables below [`DEFAULT_BULK_THRESHOLD`](bulk::DEFAULT_BULK_THRESHOLD)
    /// rows, and the bulk build above it. Use
    /// [`Self::create_spatial_index_with`] to override the threshold.
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
        self.create_spatial_index_with(BulkIndexOptions::default())
    }

    /// Build the RTree spatial index with an explicit choice of build path.
    ///
    /// Identical to [`Self::create_spatial_index`] but with a caller-supplied
    /// [`BulkIndexOptions`] controlling the bulk-vs-triggered threshold.
    /// [`BulkIndexOptions::always_bulk`] and [`BulkIndexOptions::never_bulk`]
    /// force a path.
    ///
    /// # Errors
    ///
    /// As [`Self::create_spatial_index`].
    pub fn create_spatial_index_with(&self, options: BulkIndexOptions) -> Result<()> {
        self.create_spatial_index_impl(options, bulk::no_fault)
            .map(|_| ())
    }

    /// The `create_spatial_index` core, returning which path built the index and
    /// taking a [`TestFault`] so that a test can force the bulk gate to fail.
    fn create_spatial_index_impl(
        &self,
        options: BulkIndexOptions,
        fault: TestFault,
    ) -> Result<BuildPath> {
        let geom = self.require_geometry_column()?;
        let pk = self.require_primary_key()?;
        let conn = self.gpkg().connection();
        let table = self.table_name();
        let column = &geom.column_name;
        let rtree = triggers::rtree_table_name(table, column);
        if table_exists(conn, &rtree)? {
            return Err(Error::SpatialIndexExists {
                table_name: table.to_owned(),
                column_name: column.clone(),
            });
        }

        if bulk::table_row_count(conn, table)? < options.bulk_threshold {
            let tx = conn.unchecked_transaction()?;
            create_index_in_transaction(&tx, table, column, pk)?;
            tx.commit()?;
            return Ok(BuildPath::Triggered);
        }

        // Bulk path: `fill_index` creates the virtual table, copies the scratch
        // shadow tables (or falls back), then runs `after` in the same
        // transaction to install the triggers and the extension row atomically.
        // No precomputed entry set: the table is already populated, so the
        // envelopes must come from a scan.
        bulk::fill_index(
            conn,
            table,
            column,
            pk,
            &rtree,
            options,
            None,
            fault,
            |conn| {
                for sql in triggers::create_triggers_sql(table, column, pk)? {
                    conn.execute_batch(&sql)?;
                }
                register_extension_row(conn, table, column)?;
                Ok(())
            },
        )
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
        crate::extensions::unregister(
            &tx,
            self.table_name(),
            &geom.column_name,
            triggers::EXTENSION_NAME,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Repair a legacy, inconsistent, or desynchronised RTree spatial index:
    /// install the GeoPackage 1.4 trigger set and rebuild the index content.
    ///
    /// The pre-1.4 `update1` trigger corrupts an index under `UPSERT`; 1.4
    /// renamed the fixed triggers so the repaired state is detectable by name.
    /// This is **never** invoked automatically: a rebuild is expensive, and an
    /// index this crate will not trust is one it works around rather than one
    /// that gives a wrong answer. Contrast [`Layer::extent`], which does record
    /// what it measures, for the reasons given in the module documentation.
    ///
    /// Repairs every state except a healthy or an absent index (see
    /// [`SpatialIndexStatus`]):
    ///
    /// - [`SpatialIndexStatus::Current`]: already the 1.4 set with its virtual
    ///   table, which is a no-op.
    /// - [`SpatialIndexStatus::Legacy`]: a pre-1.4 or mixed trigger set; drop
    ///   every RTree trigger, install the 1.4 set, rebuild the content.
    /// - [`SpatialIndexStatus::Stale`]: a virtual table with missing triggers
    ///   (the state an interrupted bulk build or crash mid-[`Self::write_all`]
    ///   leaves), or orphaned triggers with no table; same rebuild, so the
    ///   index is made consistent and 1.4-current again.
    /// - [`SpatialIndexStatus::Absent`]: nothing to repair:
    ///   [`Error::NoSpatialIndex`]; use [`Self::create_spatial_index`].
    ///
    /// The `gpkg_extensions` registration row is left as-is: a repair operates
    /// on an index that was already created (and so already registered).
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] on a layer with no geometry column.
    /// - [`Error::NoPrimaryKey`] if the table has no single-column primary key.
    /// - [`Error::NoSpatialIndex`] if there is no index at all to repair.
    pub fn repair_spatial_index(&self) -> Result<()> {
        let geom = self.require_geometry_column()?;
        let pk = self.require_primary_key()?;
        let conn = self.gpkg().connection();
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);
        let generation = self.classify_rtree_triggers(&geom.column_name)?;
        let rtree_exists = table_exists(conn, &rtree)?;

        // A current, complete index needs nothing.
        if generation == TriggerGeneration::V1_4 && rtree_exists {
            return Ok(());
        }
        // Truly absent: no triggers and no virtual table.
        if generation == TriggerGeneration::None && !rtree_exists {
            return Err(Error::NoSpatialIndex {
                table_name: self.table_name().to_owned(),
                column_name: geom.column_name.clone(),
            });
        }

        // Everything else is repairable by rebuilding: a legacy/mixed trigger
        // set, a stale index left by an interrupted bulk build (a virtual table
        // with no triggers), or orphaned triggers with no table.
        self.rebuild_index_impl(geom, pk)
    }

    /// Rebuild the RTree spatial index from the geometries, whatever state it
    /// is in: drop every RTree trigger, install the 1.4 set, empty the virtual
    /// table and repopulate it, in one transaction.
    ///
    /// [`Self::repair_spatial_index`] is the cheap operation and answers a
    /// structural question, so it returns without doing anything when the
    /// virtual table and the 1.4 triggers are both present. That says nothing
    /// about the entries. This is the operation for an index whose *contents*
    /// are in doubt, which [`Self::audit_spatial_index`] is how to find out.
    ///
    /// Like the repair, it is never invoked automatically, and for the same
    /// reason: this reads every geometry in the layer and rewrites the tree,
    /// which is not something to do to a caller who only asked a question.
    ///
    /// # Errors
    ///
    /// As [`Self::repair_spatial_index`], including [`Error::NoSpatialIndex`]
    /// when there is no index to rebuild ([`Self::create_spatial_index`] builds
    /// one).
    pub fn rebuild_spatial_index(&self) -> Result<()> {
        let geom = self.require_geometry_column()?;
        let pk = self.require_primary_key()?;
        let conn = self.gpkg().connection();
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);
        if !table_exists(conn, &rtree)?
            && self.classify_rtree_triggers(&geom.column_name)? == TriggerGeneration::None
        {
            return Err(Error::NoSpatialIndex {
                table_name: self.table_name().to_owned(),
                column_name: geom.column_name.clone(),
            });
        }
        self.rebuild_index_impl(geom, pk)
    }

    /// The shared rebuild: 1.4 triggers, an empty virtual table, repopulated.
    fn rebuild_index_impl(&self, geom: &GeometryColumn, pk: &str) -> Result<()> {
        let conn = self.gpkg().connection();
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);
        let tx = conn.unchecked_transaction()?;
        drop_all_rtree_triggers(&tx, self.table_name(), &geom.column_name)?;
        for sql in triggers::create_triggers_sql(self.table_name(), &geom.column_name, pk)? {
            tx.execute_batch(&sql)?;
        }
        // Ensure the virtual table exists and is empty before repopulating: it
        // is normally present, but a stale/corrupt file may have lost it.
        if table_exists(&tx, &rtree)? {
            tx.execute_batch(&format!("DELETE FROM {}", quote(&rtree)?))?;
        } else {
            tx.execute_batch(&triggers::create_rtree_table_sql(
                self.table_name(),
                &geom.column_name,
            )?)?;
        }
        tx.execute_batch(&triggers::populate_rtree_sql(
            self.table_name(),
            &geom.column_name,
            pk,
        )?)?;
        tx.commit()?;
        Ok(())
    }

    /// Compare the RTree's contents against the geometries they describe.
    ///
    /// [`Self::spatial_index_status`] asks whether the index is *there*; this
    /// asks whether it is *right*, which structure cannot answer. A file may
    /// arrive with a structurally perfect index whose entries were never
    /// populated, populated partially, or left behind by rows since deleted.
    ///
    /// This reads every geometry in the layer and is priced accordingly: it is
    /// a deliberate audit, not a check to run before each query. Nothing is
    /// written, and [`Self::rebuild_spatial_index`] is the fix for a result
    /// that is not [`SpatialIndexAudit::is_consistent`].
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] on a layer with no geometry column.
    /// - [`Error::NoPrimaryKey`] if the table has no single-column primary key,
    ///   since the index is keyed on it.
    /// - [`Error::NoSpatialIndex`] if the virtual table is absent.
    pub fn audit_spatial_index(&self) -> Result<SpatialIndexAudit> {
        let geom = self.require_geometry_column()?;
        let pk = self.require_primary_key()?;
        let conn = self.gpkg().connection();
        let rtree = quote(&triggers::rtree_table_name(
            self.table_name(),
            &geom.column_name,
        ))?;
        if !table_exists(
            conn,
            &triggers::rtree_table_name(self.table_name(), &geom.column_name),
        )? {
            return Err(Error::NoSpatialIndex {
                table_name: self.table_name().to_owned(),
                column_name: geom.column_name.clone(),
            });
        }
        let table = quote(self.table_name())?;
        let column = quote(&geom.column_name)?;
        let key = quote(pk)?;

        // One pass over the indexable rows: how many there are, how many have
        // no entry, and how many have one that fails to cover them. The RTree
        // stores `f32` rounded outward, so a covering entry can be larger than
        // the geometry but never smaller.
        let (indexable, missing, not_covering): (i64, i64, i64) = conn.query_row(
            &format!(
                "SELECT count(*),                  sum(CASE WHEN r.id IS NULL THEN 1 ELSE 0 END),                  sum(CASE WHEN r.id IS NOT NULL AND (                    r.minx > ST_MinX(f.{column}) OR r.maxx < ST_MaxX(f.{column}) OR                    r.miny > ST_MinY(f.{column}) OR r.maxy < ST_MaxY(f.{column})                  ) THEN 1 ELSE 0 END)                  FROM {table} f LEFT JOIN {rtree} r ON r.id = f.{key}                  WHERE f.{column} IS NOT NULL AND NOT ST_IsEmpty(f.{column})"
            ),
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    // sum() over no rows is NULL rather than 0.
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                ))
            },
        )?;

        let (entries, extra): (i64, i64) = conn.query_row(
            &format!(
                "SELECT count(*),                  sum(CASE WHEN NOT EXISTS (                    SELECT 1 FROM {table} f WHERE f.{key} = r.id                    AND f.{column} IS NOT NULL AND NOT ST_IsEmpty(f.{column})                  ) THEN 1 ELSE 0 END)                  FROM {rtree} r"
            ),
            [],
            |row| Ok((row.get(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )?;

        let count = |n: i64| usize::try_from(n).unwrap_or(0);
        Ok(SpatialIndexAudit {
            indexable: count(indexable),
            entries: count(entries),
            missing: count(missing),
            extra: count(extra),
            not_covering: count(not_covering),
        })
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
/// Create the RTree virtual table, install the GeoPackage 1.4 trigger set,
/// populate it from the table's existing rows, and register the extension.
///
/// Every statement runs on `conn`, which the caller must already have inside a
/// transaction, and nothing is committed here. Two callers want that: the
/// triggered branch of [`Layer::create_spatial_index`], which opens its own, and
/// [`crate::GeoPackage::create_layer`], which builds the layer and its index in
/// one transaction so a failure cannot leave a table behind without one.
///
/// The population statement is a no-op on the fresh, empty table `create_layer`
/// hands it.
pub(crate) fn create_index_in_transaction(
    conn: &Connection,
    table: &str,
    column: &str,
    pk: &str,
) -> Result<()> {
    conn.execute_batch(&triggers::create_rtree_table_sql(table, column)?)?;
    for sql in triggers::create_triggers_sql(table, column, pk)? {
        conn.execute_batch(&sql)?;
    }
    conn.execute_batch(&triggers::populate_rtree_sql(table, column, pk)?)?;
    register_extension_row(conn, table, column)
}

fn register_extension_row(conn: &Connection, table: &str, column: &str) -> Result<()> {
    crate::extensions::register(
        conn,
        Some(table),
        Some(column),
        triggers::EXTENSION_NAME,
        triggers::EXTENSION_DEFINITION,
        triggers::EXTENSION_SCOPE,
    )
}

/// Drop every RTree trigger for `table`/`column`, of any generation, by name.
///
/// Triggers fire on the user table, so their `sqlite_master.tbl_name` is
/// `table`; the ones belonging to this index share the `rtree_<table>_<column>_`
/// name prefix (the same prefix rule [`triggers::classify_triggers`] uses).
///
/// Shared with the bulk `write_all` path ([`crate::writer`]), which drops the
/// triggers before a bulk insert so the index is not maintained per-row, then
/// reinstalls them after the bulk rebuild.
pub(crate) fn drop_all_rtree_triggers(conn: &Connection, table: &str, column: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeoPackage, GeometrySpec, TableSchemaBuilder};
    use geo_types::Point;
    use geopackage_core::types::GeometryType;

    /// A GeoPackage with a `pts(fid, geom)` point layer populated via the
    /// writer. Coordinates are chosen exact-in-`f32` so the index bounds equal
    /// the `ST_*` envelope scan.
    fn populated(points: &[(i64, f64, f64)]) -> (tempfile::TempDir, GeoPackage) {
        let dir = tempfile::tempdir().unwrap();
        let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
        let builder = TableSchemaBuilder::new("pts")
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            // These tests drive the index lifecycle themselves.
            .spatial_index(false);
        let layer = gpkg.create_layer(&builder).unwrap();
        let mut writer = layer.writer().unwrap();
        for &(fid, x, y) in points {
            writer.insert(Some(fid), &Point::new(x, y), &[]).unwrap();
        }
        writer.commit().unwrap();
        (dir, gpkg)
    }

    /// Whether the rtree contents equal a manual `ST_*` envelope scan.
    fn rtree_matches_scan(gpkg: &GeoPackage) -> bool {
        let conn = gpkg.connection();
        let read = |sql: &str| -> Vec<(i64, f64, f64, f64, f64)> {
            let mut stmt = conn.prepare(sql).unwrap();
            stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        read("SELECT id, minx, maxx, miny, maxy FROM rtree_pts_geom ORDER BY id")
            == read(
                "SELECT fid, ST_MinX(geom), ST_MaxX(geom), ST_MinY(geom), ST_MaxY(geom) \
                 FROM pts WHERE geom NOT NULL AND NOT ST_IsEmpty(geom) ORDER BY fid",
            )
    }

    /// A [`TestFault`] that deletes a row from the written index, so it no longer
    /// matches the accumulated set and the gate must reject it.
    fn corrupt_written_index(conn: &Connection, rtree: &str) -> Result<()> {
        conn.execute_batch(&format!(
            "DELETE FROM {} WHERE id = (SELECT min(id) FROM {})",
            quote(rtree)?,
            quote(rtree)?
        ))?;
        Ok(())
    }

    /// A [`TestFault`] that shrinks one stored bound so it no longer contains
    /// the geometry it indexes, leaving the row count and the set of ids
    /// intact. The gate has to match the row to its accumulated entry and
    /// compare the bounds to catch this.
    fn shrink_one_stored_bound(conn: &Connection, rtree: &str) -> Result<()> {
        conn.execute_batch(&format!(
            // Moved rather than inverted: the rtree itself rejects `minx > maxx`,
            // so the box stays well formed and simply sits somewhere else.
            "UPDATE {} SET minx = 1e6, maxx = 1e6 WHERE id = (SELECT max(id) FROM {})",
            quote(rtree)?,
            quote(rtree)?
        ))?;
        Ok(())
    }

    #[test]
    fn bulk_path_builds_a_correct_index() {
        let (_dir, gpkg) = populated(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
        let layer = gpkg.layer("pts").unwrap();
        let path = layer
            .create_spatial_index_impl(BulkIndexOptions::always_bulk(), bulk::no_fault)
            .unwrap();
        assert_eq!(path, BuildPath::Bulk);
        assert!(layer.has_spatial_index().unwrap());
        assert!(rtree_matches_scan(&gpkg));
    }

    #[test]
    fn below_threshold_uses_the_triggered_path() {
        let (_dir, gpkg) = populated(&[(1, 1.0, 1.0)]);
        let layer = gpkg.layer("pts").unwrap();
        let path = layer
            .create_spatial_index_impl(BulkIndexOptions::never_bulk(), bulk::no_fault)
            .unwrap();
        assert_eq!(path, BuildPath::Triggered);
        assert!(rtree_matches_scan(&gpkg));
    }

    #[test]
    fn corrupt_scratch_falls_back_to_the_triggered_path() {
        let (_dir, gpkg) = populated(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
        let layer = gpkg.layer("pts").unwrap();
        let path = layer
            .create_spatial_index_impl(BulkIndexOptions::always_bulk(), corrupt_written_index)
            .unwrap();
        // The gate rejected the corrupted index and rebuilt it through the
        // triggers.
        assert_eq!(path, BuildPath::TriggeredFallback);
        assert!(layer.has_spatial_index().unwrap());
        // The fallback still produced a correct index.
        assert!(rtree_matches_scan(&gpkg));
    }

    #[test]
    fn stored_bounds_that_exclude_their_geometry_fall_back() {
        let (_dir, gpkg) = populated(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
        let layer = gpkg.layer("pts").unwrap();
        let path = layer
            .create_spatial_index_impl(BulkIndexOptions::always_bulk(), shrink_one_stored_bound)
            .unwrap();
        // Every id is still present and the count still matches, so the gate
        // only rejects this by comparing each row's bounds against the entry it
        // indexes.
        assert_eq!(path, BuildPath::TriggeredFallback);
        assert!(rtree_matches_scan(&gpkg));
    }

    #[test]
    fn status_classifies_absent_and_current() {
        let (_dir, gpkg) = populated(&[(1, 1.0, 1.0)]);
        let layer = gpkg.layer("pts").unwrap();
        assert_eq!(
            layer.spatial_index_status().unwrap(),
            SpatialIndexStatus::Absent
        );
        layer.create_spatial_index().unwrap();
        assert_eq!(
            layer.spatial_index_status().unwrap(),
            SpatialIndexStatus::Current
        );
    }

    /// Dropping the triggers while leaving the virtual table is exactly the file
    /// state an interrupted bulk build (or a crash mid-`write_all`) leaves: the
    /// rows are committed, the rtree table is present but no longer maintained.
    /// The status detector must flag it `Stale`, the read gate must decline it,
    /// and `repair_spatial_index` must rebuild it to `Current`.
    #[test]
    fn stale_index_is_detected_and_repaired() {
        let (_dir, gpkg) = populated(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
        let layer = gpkg.layer("pts").unwrap();
        layer.create_spatial_index().unwrap();
        assert!(rtree_matches_scan(&gpkg));

        // Simulate the interrupted-bulk-build window: triggers gone, table kept.
        drop_all_rtree_triggers(gpkg.connection(), "pts", "geom").unwrap();

        assert_eq!(
            layer.spatial_index_status().unwrap(),
            SpatialIndexStatus::Stale
        );
        // The read path declines a stale index (falls back to a full scan)
        // rather than trusting desynced contents.
        assert!(!layer.has_spatial_index().unwrap());

        // repair rebuilds and reinstalls the 1.4 triggers.
        layer.repair_spatial_index().unwrap();
        assert_eq!(
            layer.spatial_index_status().unwrap(),
            SpatialIndexStatus::Current
        );
        assert!(layer.has_spatial_index().unwrap());
        assert!(rtree_matches_scan(&gpkg));
    }

    #[test]
    fn repair_absent_index_errors() {
        let (_dir, gpkg) = populated(&[(1, 1.0, 1.0)]);
        let layer = gpkg.layer("pts").unwrap();
        assert!(matches!(
            layer.repair_spatial_index(),
            Err(Error::NoSpatialIndex { .. })
        ));
    }
}
