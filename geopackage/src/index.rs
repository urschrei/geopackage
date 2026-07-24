//! RTree spatial-index lifecycle on a [`Layer`]: [`Layer::create_spatial_index`],
//! [`Layer::drop_spatial_index`], and [`Layer::repair_spatial_index`].
//!
//! The normative SQL (the `rtree_<table>_<column>` virtual table, the
//! GeoPackage 1.4 trigger set, and the population statement) is emitted by
//! [`geopackage_core::triggers`] (spec Annex F.3, reproduced verbatim there).
//! This module drives it against the live connection and maintains the
//! `gpkg_extensions` registration row (spec Annex F.3 requirements 75/76).
//!
//! Design decision D7: a new index always gets the 1.4 trigger set
//! (`update5`/`update6`/`update7`), which is UPSERT-safe. Older generations are
//! never repaired automatically: [`Layer::repair_spatial_index`] is the sole,
//! explicitly user-invoked path that rewrites an existing trigger set.
//!
//! Population takes one of two paths (design decision D8). Below the
//! [`BulkIndexOptions`] threshold it is a single `INSERT INTO rtree SELECT` over
//! the existing rows, using the registered `ST_*` functions and skipping
//! empty/NULL geometries exactly as the triggers do. At or above the threshold
//! it is the bulk shadow-table build in [`crate::bulk`]: accumulate the
//! envelopes, build the tree in memory, and write the shadow tables directly,
//! gated with automatic fallback to the triggered path.

use geopackage_core::ddl;
use geopackage_core::ident::quote;
use geopackage_core::triggers::{self, TriggerGeneration};
use rusqlite::Connection;

use crate::bulk::{self, BuildPath, BulkIndexOptions, ScratchTamper};
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
    /// `UPSERT`; [`Layer::repair_spatial_index`] upgrades it to the 1.4 set
    /// (design decision D7).
    Legacy,
    /// A desynchronised index: the virtual table exists but its triggers are
    /// missing (or triggers exist with no table). The state an interrupted bulk
    /// build leaves: for example a crash during [`Layer::write_all`] after the
    /// rows commit but before the index is rebuilt, since the `ATTACH` the bulk
    /// build needs cannot join that final transaction (design decision D8).
    ///
    /// The index is **not** silently trusted: [`Layer::has_spatial_index`]
    /// reports `false` for it, so [`Layer::features_in`] falls back to a correct
    /// full scan. [`Layer::repair_spatial_index`] rebuilds it and reinstalls the
    /// 1.4 triggers.
    Stale,
}

impl Layer<'_> {
    /// Classify this layer's RTree spatial index (see [`SpatialIndexStatus`]).
    ///
    /// A layer with no geometry column is [`SpatialIndexStatus::Absent`]. This
    /// is the detector for the interrupted-bulk-build case (design decision D8):
    /// a [`SpatialIndexStatus::Stale`] result directs the caller to
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
    /// GeoPackage 1.4 trigger set (design decision D7), populates the index from
    /// the existing rows (skipping NULL and empty geometries), and registers the
    /// `gpkg_rtree_index` extension in `gpkg_extensions` (creating that table on
    /// first use). The whole operation is one transaction.
    ///
    /// Population takes the per-row triggered path (a single
    /// `INSERT INTO rtree SELECT` driven by the registered `ST_*` functions) for
    /// tables below [`DEFAULT_BULK_THRESHOLD`](bulk::DEFAULT_BULK_THRESHOLD)
    /// rows, and the D8 bulk shadow-table build above it. Use
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

    /// Build the RTree spatial index with an explicit choice of build path
    /// (design decision D8).
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
        self.create_spatial_index_impl(options, bulk::no_tamper)
            .map(|_| ())
    }

    /// The `create_spatial_index` core, returning which path built the index and
    /// taking a scratch-tamper seam so tests can force the bulk gate to fail.
    fn create_spatial_index_impl(
        &self,
        options: BulkIndexOptions,
        tamper: ScratchTamper,
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
            tx.execute_batch(&triggers::create_rtree_table_sql(table, column)?)?;
            for sql in triggers::create_triggers_sql(table, column, pk)? {
                tx.execute_batch(&sql)?;
            }
            tx.execute_batch(&triggers::populate_rtree_sql(table, column, pk)?)?;
            register_extension_row(&tx, table, column)?;
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
            tamper,
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

    /// Repair a legacy, inconsistent, or desynchronised RTree spatial index:
    /// install the GeoPackage 1.4 trigger set and rebuild the index content
    /// (design decisions D7, D8).
    ///
    /// The pre-1.4 `update1` trigger corrupts an index under `UPSERT`; 1.4
    /// renamed the fixed triggers so the repaired state is detectable by name.
    /// This is **never** invoked automatically, since reading a file never mutates
    /// it.
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
        // set (D7), a stale index left by an interrupted bulk build (a virtual
        // table with no triggers, D8), or orphaned triggers with no table.
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
    use crate::packed::PackedRtree;
    use crate::{GeoPackage, GeometrySpec, TableSchemaBuilder};
    use geo_types::Point;
    use geopackage_core::types::GeometryType;

    /// A GeoPackage with a `pts(fid, geom)` point layer populated via the
    /// writer. Coordinates are chosen exact-in-`f32` so the index bounds equal
    /// the `ST_*` envelope scan.
    fn populated(points: &[(i64, f64, f64)]) -> (tempfile::TempDir, GeoPackage) {
        let dir = tempfile::tempdir().unwrap();
        let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
        let builder =
            TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326));
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

    /// A test tamper that drops an entry from the packed tree's rowid mapping,
    /// so the written result no longer matches the accumulated set and the gate
    /// must reject it.
    fn corrupt_packed_tree(packed: &mut PackedRtree) -> Result<()> {
        packed.rowid_map.pop();
        Ok(())
    }

    #[test]
    fn bulk_path_builds_a_correct_index() {
        let (_dir, gpkg) = populated(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
        let layer = gpkg.layer("pts").unwrap();
        let path = layer
            .create_spatial_index_impl(BulkIndexOptions::always_bulk(), bulk::no_tamper)
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
            .create_spatial_index_impl(BulkIndexOptions::never_bulk(), bulk::no_tamper)
            .unwrap();
        assert_eq!(path, BuildPath::Triggered);
        assert!(rtree_matches_scan(&gpkg));
    }

    #[test]
    fn corrupt_scratch_falls_back_to_the_triggered_path() {
        let (_dir, gpkg) = populated(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
        let layer = gpkg.layer("pts").unwrap();
        let path = layer
            .create_spatial_index_impl(BulkIndexOptions::always_bulk(), corrupt_packed_tree)
            .unwrap();
        // The gate rejected the tampered bulk copy and rebuilt through triggers.
        assert_eq!(path, BuildPath::TriggeredFallback);
        assert!(layer.has_spatial_index().unwrap());
        // The fallback still produced a correct index.
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
