//! D8 bulk RTree build: the scratch-database shadow-table technique.
//!
//! SQLite's RTree module has no bulk-load entry point, so populating an index
//! one row at a time, whether through the spec's `INSERT INTO rtree SELECT`
//! statement or through the per-row triggers, pays the node-splitting cost for
//! every row. GDAL's fix, which this module reimplements from the issue
//! description ([gdal#7614](https://github.com/OSGeo/gdal/issues/7614)), is to
//! build the RTree in a **scratch in-memory database** and then copy its
//! `rtree_%_node` / `_rowid` / `_parent` shadow tables verbatim into the target.
//! The copy is a plain B-tree table copy: no RTree module logic, no `ST_*`
//! calls, no triggers.
//!
//! The GeoPackage RTree is always `rtree(id, minx, maxx, miny, maxy)` (2-D, no
//! auxiliary columns), so the three shadow tables have a fixed shape
//! (`_node(nodeno, data)`, `_rowid(rowid, nodeno)`, `_parent(nodeno,
//! parentnode)`) and copying between two such tables is well defined regardless
//! of the RTree's name.
//!
//! Every bulk build is **gated** before it is trusted (see [`gate`]): the copied
//! index must contain exactly the accumulated `(fid, envelope)` set (row count
//! plus a per-row containment check), and it must pass a structural check
//! ([`StructuralCheck`]: `rtreecheck()` on the index by default, optionally a
//! whole-database `PRAGMA integrity_check`). Any anomaly makes [`fill_index`]
//! fall back to the triggered population statement, so a failed gate never
//! yields a corrupt or stale index, only a slower build.
//!
//! The entry set itself comes either from an `ST_*` scan of the table
//! ([`fill_index`]) or, when the caller can prove it accounts for every
//! indexable row, from envelopes computed while encoding the geometries
//! ([`fill_index_with`]).

use std::collections::HashMap;

use geopackage_core::ident::quote;
use geopackage_core::triggers;
use rusqlite::Connection;

use crate::Result;

/// Default candidate-row count at or above which
/// [`crate::Layer::create_spatial_index`] and [`crate::Layer::write_all`] choose
/// the bulk shadow-table build over the per-row triggered build.
///
/// Below this, the fixed cost of the scratch database, the shadow-table copy,
/// and the `PRAGMA integrity_check` gate outweighs the saving; above it the
/// bulk copy wins. Override with [`BulkIndexOptions`].
pub const DEFAULT_BULK_THRESHOLD: usize = 10_000;

/// Tuning for the RTree bulk-build path (design decision D8).
///
/// Passed to [`crate::Layer::create_spatial_index_with`] (and the bulk
/// `write_all` path). The default threshold is [`DEFAULT_BULK_THRESHOLD`];
/// [`Self::always_bulk`] and [`Self::never_bulk`] force a path (mainly for tests
/// and benchmarking).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct BulkIndexOptions {
    /// Candidate-row count at or above which the bulk build is used. `0` always
    /// uses the bulk path; [`usize::MAX`] always uses the triggered path.
    pub bulk_threshold: usize,
    /// How thoroughly the gate checks database structure after the copy.
    /// Defaults to [`StructuralCheck::RtreeOnly`].
    pub structural_check: StructuralCheck,
}

/// How much of the database the bulk-build gate checks structurally after
/// copying the shadow tables.
///
/// Both settings run the full content gate (the bijection between the copied
/// index and the accumulated envelope set, and the per-row containment check);
/// this chooses only the SQLite-level structural check layered on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum StructuralCheck {
    /// Check the RTree that was just built, with `rtreecheck()`. Its cost is
    /// proportional to the index rather than to the whole file, and a
    /// pre-existing problem elsewhere in the database cannot force a needless
    /// fallback. The default.
    #[default]
    RtreeOnly,
    /// Additionally run a whole-database `PRAGMA integrity_check`. This is
    /// O(database): on a large file it dominates the gate, and a benign
    /// pre-existing issue anywhere in the file forces the (still correct)
    /// triggered fallback. Use when validating a file of unknown provenance.
    FullDatabase,
}

impl Default for BulkIndexOptions {
    fn default() -> Self {
        Self {
            bulk_threshold: DEFAULT_BULK_THRESHOLD,
            structural_check: StructuralCheck::RtreeOnly,
        }
    }
}

impl BulkIndexOptions {
    /// Options with an explicit bulk threshold.
    pub fn with_threshold(bulk_threshold: usize) -> Self {
        Self {
            bulk_threshold,
            ..Self::default()
        }
    }

    /// Always take the bulk path (threshold `0`).
    pub fn always_bulk() -> Self {
        Self::with_threshold(0)
    }

    /// Never take the bulk path (threshold [`usize::MAX`]).
    pub fn never_bulk() -> Self {
        Self::with_threshold(usize::MAX)
    }

    /// Set the structural check the gate runs after the copy.
    #[must_use]
    pub fn with_structural_check(mut self, structural_check: StructuralCheck) -> Self {
        self.structural_check = structural_check;
        self
    }
}

/// Which path produced an index's contents. Returned by the internal build
/// entry points so tests can assert the bulk path (and its fallback) were
/// actually exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildPath {
    /// The per-row triggered population (`INSERT INTO rtree SELECT`), chosen
    /// because the table was below the bulk threshold.
    Triggered,
    /// The D8 bulk shadow-table copy, gate passed.
    Bulk,
    /// The bulk copy was attempted but its gate failed, so the triggered
    /// population was used as a fallback.
    TriggeredFallback,
}

/// A test seam run against the scratch database after it is built and before its
/// shadow tables are copied into the target. Production always passes
/// [`no_tamper`]; a test can pass a function that corrupts the scratch state to
/// prove the gate rejects it and the triggered fallback still yields a correct
/// index.
pub(crate) type ScratchTamper = fn(&ScratchDb<'_>) -> Result<()>;

/// The no-op [`ScratchTamper`] used in production.
pub(crate) fn no_tamper(_: &ScratchDb<'_>) -> Result<()> {
    Ok(())
}

/// Alias of the attached scratch database, and the name of the scratch RTree
/// built inside it. Fixed names are safe because index building is
/// single-threaded on one connection and the attach is bracketed by
/// [`ScratchDb`]'s lifetime.
const SCRATCH_ALIAS: &str = "gpkg_bulk_scratch";
const SCRATCH_RTREE: &str = "gpkg_bulk_rtree";

/// An attached in-memory scratch database holding a freshly built RTree, whose
/// shadow tables are copied into the target. Detaches on drop.
pub(crate) struct ScratchDb<'c> {
    conn: &'c Connection,
}

impl<'c> ScratchDb<'c> {
    /// Attach a fresh in-memory scratch database and create the scratch RTree.
    fn attach(conn: &'c Connection) -> Result<Self> {
        // Drop any alias left attached by an earlier build that failed to
        // detach, so the ATTACH below cannot fail on a stale name.
        best_effort(conn, &format!("DETACH DATABASE {}", quote(SCRATCH_ALIAS)?));
        conn.execute_batch(&format!(
            "ATTACH DATABASE ':memory:' AS {}",
            quote(SCRATCH_ALIAS)?
        ))?;
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE {} USING rtree(id, minx, maxx, miny, maxy)",
            self_scratch_rtree()?
        ))?;
        Ok(Self { conn })
    }

    /// Insert the accumulated `(fid, envelope)` rows into the scratch RTree.
    fn build(&self, rows: &[(i64, [f64; 4])]) -> Result<()> {
        let sql = format!(
            "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5)",
            self_scratch_rtree()?
        );
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = self.conn.prepare(&sql)?;
            for (id, [min_x, max_x, min_y, max_y]) in rows {
                stmt.execute(rusqlite::params![id, min_x, max_x, min_y, max_y])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert one raw row into the scratch RTree. Only used by test tampers to
    /// corrupt the scratch state (e.g. add a row so the copied index no longer
    /// matches the accumulated set).
    #[cfg(test)]
    pub(crate) fn insert_scratch_row(
        &self,
        id: i64,
        [min_x, max_x, min_y, max_y]: [f64; 4],
    ) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO {} VALUES (?1, ?2, ?3, ?4, ?5)",
                self_scratch_rtree()?
            ),
            rusqlite::params![id, min_x, max_x, min_y, max_y],
        )?;
        Ok(())
    }
}

impl Drop for ScratchDb<'_> {
    fn drop(&mut self) {
        if let Ok(alias) = quote(SCRATCH_ALIAS) {
            best_effort(self.conn, &format!("DETACH DATABASE {alias}"));
        }
    }
}

/// Run a housekeeping statement whose failure is safe to ignore: a failed
/// ATTACH/DETACH leaves an in-memory scratch database that is discarded when the
/// connection closes.
fn best_effort(conn: &Connection, sql: &str) {
    if conn.execute_batch(sql).is_err() {
        // Deliberately ignored: this is best-effort cleanup.
    }
}

/// The quoted, alias-qualified scratch RTree name (`"alias"."rtree"`).
fn self_scratch_rtree() -> Result<String> {
    Ok(format!(
        "{}.{}",
        quote(SCRATCH_ALIAS)?,
        quote(SCRATCH_RTREE)?
    ))
}

/// The number of rows in `table` (the cheap decision proxy: no `ST_*` calls).
pub(crate) fn table_row_count(conn: &Connection, table: &str) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {}", quote(table)?),
        [],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// Accumulate `(fid, [min_x, max_x, min_y, max_y])` for every row whose geometry
/// is indexable, using the registered `ST_*` functions and the exact NULL/empty
/// guard the trigger population uses, so the accumulated set is identical to
/// what the triggered path would index.
fn accumulate_envelopes(
    conn: &Connection,
    table: &str,
    geom: &str,
    pk: &str,
) -> Result<Vec<(i64, [f64; 4])>> {
    let (t, c, i) = (quote(table)?, quote(geom)?, quote(pk)?);
    let sql = format!(
        "SELECT {i}, ST_MinX({c}), ST_MaxX({c}), ST_MinY({c}), ST_MaxY({c}) \
         FROM {t} WHERE {c} NOT NULL AND NOT ST_IsEmpty({c})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?],
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Copy the three shadow tables of the scratch RTree into the target RTree's
/// shadow tables, replacing their (freshly created, empty) contents.
fn copy_shadow_tables(conn: &Connection, rtree: &str) -> Result<()> {
    for suffix in ["node", "rowid", "parent"] {
        let target = quote(&format!("{rtree}_{suffix}"))?;
        let source = format!(
            "{}.{}",
            quote(SCRATCH_ALIAS)?,
            quote(&format!("{SCRATCH_RTREE}_{suffix}"))?
        );
        conn.execute_batch(&format!(
            "DELETE FROM {target}; INSERT INTO {target} SELECT * FROM {source};"
        ))?;
    }
    Ok(())
}

/// Gate a freshly copied RTree against the accumulated `(fid, envelope)` set.
///
/// Passes only when the index contains exactly one row per accumulated entry
/// (row count and a bijection on `id`), each stored bound conservatively
/// contains the true envelope (the RTree stores `f32` bounds, minima rounded
/// down and maxima rounded up, so containment, not equality, is the correct
/// relation), and `PRAGMA integrity_check` reports `ok`.
fn gate(
    conn: &Connection,
    rtree: &str,
    mut expected: HashMap<i64, [f64; 4]>,
    structural_check: StructuralCheck,
) -> Result<bool> {
    let quoted = quote(rtree)?;
    let count: i64 = conn.query_row(&format!("SELECT count(*) FROM {quoted}"), [], |r| r.get(0))?;
    if usize::try_from(count).unwrap_or(usize::MAX) != expected.len() {
        return Ok(false);
    }

    let mut stmt = conn.prepare(&format!("SELECT id, minx, maxx, miny, maxy FROM {quoted}"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let (s_min_x, s_max_x, s_min_y, s_max_y): (f64, f64, f64, f64) =
            (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
        let Some([min_x, max_x, min_y, max_y]) = expected.remove(&id) else {
            return Ok(false);
        };
        if !(s_min_x <= min_x && s_max_x >= max_x && s_min_y <= min_y && s_max_y >= max_y) {
            return Ok(false);
        }
    }
    if !expected.is_empty() {
        return Ok(false);
    }

    // Structural check on top of the content gate above. `rtreecheck` walks the
    // index just built, cross-checking every entry against its parent's bounds
    // and the `%_rowid`/`%_parent` mappings against `%_node`, and reports `ok`
    // or a description of what is wrong. Its cost is proportional to the index;
    // `PRAGMA integrity_check` is O(database) and opt-in (design decision D8,
    // issue #16).
    let rtree_report: String = conn.query_row("SELECT rtreecheck(?1)", [rtree], |r| r.get(0))?;
    if rtree_report != "ok" {
        return Ok(false);
    }

    if structural_check == StructuralCheck::FullDatabase {
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        return Ok(integrity == "ok");
    }
    Ok(true)
}

/// Build (or rebuild) the RTree `rtree` for `table`/`geom` via the D8 bulk
/// shadow-table copy, gated with automatic fallback to the triggered
/// population.
///
/// On entry the virtual table may or may not exist and the RTree triggers must
/// **not** be installed (the caller drops them first for a rebuild); this
/// function (re)creates the virtual table empty, fills it, and runs `after`
/// inside the same transaction. The caller uses `after` to install the trigger
/// set and any `gpkg_extensions` row, so the whole operation commits atomically.
///
/// `precomputed` supplies the `(fid, envelope)` entry set directly instead of
/// deriving it from an `ST_*` scan of the table. Pass `Some` only when the
/// caller can prove it holds an entry for every indexable row: the bulk
/// `write_all` path passes the envelopes it computed while encoding, and only
/// when the table was empty before the write. An incomplete set would build an
/// index missing rows, which the gate cannot detect because it checks the index
/// against this very set. `None` scans, which is always sound.
///
/// `tamper` is [`no_tamper`] outside tests.
#[expect(
    clippy::too_many_arguments,
    reason = "internal build entry point threading the whole build context; a parameter struct would be used by these two call sites alone"
)]
pub(crate) fn fill_index<F>(
    conn: &Connection,
    table: &str,
    geom: &str,
    pk: &str,
    rtree: &str,
    options: BulkIndexOptions,
    precomputed: Option<Vec<(i64, [f64; 4])>>,
    tamper: ScratchTamper,
    after: F,
) -> Result<BuildPath>
where
    F: FnOnce(&Connection) -> Result<()>,
{
    let accumulated = match precomputed {
        Some(entries) => entries,
        None => accumulate_envelopes(conn, table, geom, pk)?,
    };
    let scratch = ScratchDb::attach(conn)?;
    scratch.build(&accumulated)?;
    tamper(&scratch)?;

    let quoted_rtree = quote(rtree)?;
    let create_vtab = triggers::create_rtree_table_sql(table, geom)?;

    // ATTACH/DETACH bracket this transaction (they require autocommit); the
    // scratch build above and the detach on `scratch` drop happen outside it.
    let tx = conn.unchecked_transaction()?;
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {quoted_rtree}"))?;
    conn.execute_batch(&create_vtab)?;
    copy_shadow_tables(conn, rtree)?;

    let expected: HashMap<i64, [f64; 4]> = accumulated.into_iter().collect();
    let path = if gate(conn, rtree, expected, options.structural_check)? {
        BuildPath::Bulk
    } else {
        // Discard the copied result and rebuild through the triggered
        // population, which cannot be affected by a bad shadow-table copy.
        conn.execute_batch(&format!("DROP TABLE {quoted_rtree}"))?;
        conn.execute_batch(&create_vtab)?;
        conn.execute_batch(&triggers::populate_rtree_sql(table, geom, pk)?)?;
        BuildPath::TriggeredFallback
    };

    after(conn)?;
    tx.commit()?;
    Ok(path)
}
