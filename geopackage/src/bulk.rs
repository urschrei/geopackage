//! Bulk RTree build: construct the index directly and write it.
//!
//! SQLite's RTree module has no bulk-load entry point, so populating an index
//! one row at a time, whether through the spec's `INSERT INTO rtree SELECT`
//! statement or through the per-row triggers, pays the node-splitting cost for
//! every row ([gdal#7614](https://github.com/OSGeo/gdal/issues/7614) is the
//! same observation from GDAL's side). This module first answered that by
//! building the RTree in an `ATTACH`ed scratch database and copying its shadow
//! tables into the target, but measurement showed the scratch build was then the
//! dominant cost, because it still inserted every entry through the module one
//! row at a time.
//!
//! So the tree is now built outright: [`crate::packed`] lays out the
//! `rtree_%_node` / `_rowid` / `_parent` contents in memory from the entry set,
//! and this module writes them as ordinary rows. No RTree module logic, no
//! `ST_*` calls, no triggers, and no scratch database.
//!
//! Dropping the scratch database also removed the `ATTACH`, which required
//! autocommit and so forced the build out of the caller's transaction. The
//! whole build is now one transaction.
//!
//! The GeoPackage RTree is always `rtree(id, minx, maxx, miny, maxy)` (2-D, no
//! auxiliary columns), so the three shadow tables have a fixed shape
//! (`_node(nodeno, data)`, `_rowid(rowid, nodeno)`, `_parent(nodeno,
//! parentnode)`) regardless of the RTree's name.
//!
//! # The gate
//!
//! Every bulk build is **gated** before it is trusted (see [`gate`]): the written
//! index must contain exactly the accumulated `(fid, envelope)` set (row count
//! plus a per-row containment check), and it must pass a structural check
//! ([`StructuralCheck`]: `rtreecheck()` on the index by default, optionally a
//! whole-database `PRAGMA integrity_check`). Any anomaly makes [`fill_index`]
//! fall back to the triggered population statement, so a failed gate never
//! yields a corrupt or stale index, only a slower build.
//!
//! **The gate is about 45% of the build.** Profiled over 1M points, it costs
//! ~745 ms of a ~1593 ms build: roughly half in the bijection scan, which reads
//! every entry back out of the index, and half in `rtreecheck`, which walks the
//! whole tree. GDAL's builder runs no equivalent, and without the gate this
//! crate would be comfortably faster than it rather than level with it (see
//! `roadmap/benchmarks/2026-07-24-gdal-like-for-like.md`).
//!
//! That cost is deliberate and stays for now. This module writes a tree by hand
//! into an on-disk format that SQLite does not document as an interface, so the
//! result is checked against SQLite's own checker and against the input before
//! anything relies on it. Removing or defaulting-off the gate is a question for
//! 1.0, once the packer has enough history behind it to justify the trust; it is
//! not a change to make while the format handling is this new. Until then the
//! honest framing is that roughly half of a bulk index build is insurance, and
//! the reason to keep paying it is confidence rather than speed.
//!
//! The entry set itself comes either from an `ST_*` scan of the table or, when
//! the caller can prove it accounts for every indexable row, from envelopes
//! computed while encoding the geometries (see [`fill_index`]'s `precomputed`).

use geopackage_core::geometry;
use geopackage_core::ident::quote;
use geopackage_core::triggers;
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::Result;
use crate::packed::{self, NodeSink};

/// Default candidate-row count at or above which
/// [`crate::Layer::create_spatial_index`] and [`crate::Layer::write_all`] choose
/// the bulk shadow-table build over the per-row triggered build.
///
/// Below this, the fixed cost of building and writing the tree plus the gate
/// outweighs the saving; above it the bulk build wins. Override with
/// [`BulkIndexOptions`].
pub const DEFAULT_BULK_THRESHOLD: usize = 10_000;

/// Default fraction of each RTree node's capacity used by the bulk build.
///
/// Packing full gives the smallest tree, the shallowest descent and the best
/// queries, which is what a bulk load wants. The cost is that a full node has
/// no room for a later insert, so the first append into it splits immediately.
/// Appends after a bulk load go through the triggers, which is the per-row path
/// this build exists to avoid, so the default favours the load and the queries.
/// Lower it with [`BulkIndexOptions::with_fill_factor`] when a freshly built
/// index will be appended to heavily.
pub const DEFAULT_FILL_FACTOR: f64 = 1.0;

/// Tuning for the RTree bulk-build path.
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
    ///
    /// For [`crate::Layer::write_all`] this also bounds a buffer: a source that
    /// does not advertise its length has rows held back, up to this many, to
    /// find out whether it reaches the threshold. Raising the threshold raises
    /// that bound, so a very large value paired with a lazy iterator can hold a
    /// correspondingly large number of rows in memory. `usize::MAX` buffers
    /// nothing, because it disables the path outright.
    pub bulk_threshold: usize,
    /// How thoroughly the gate checks database structure after the copy.
    /// Defaults to [`StructuralCheck::RtreeOnly`].
    pub structural_check: StructuralCheck,
    /// Fraction of each RTree node's capacity to fill when packing, in
    /// `(0, 1]`. Defaults to [`DEFAULT_FILL_FACTOR`].
    pub fill_factor: f64,
}

/// How much of the database the bulk-build gate checks structurally after
/// writing the shadow tables.
///
/// Both settings run the full content gate (the bijection between the written
/// index and the accumulated envelope set, and the per-row containment check);
/// this chooses only the SQLite-level structural check layered on top.
///
/// The gate as a whole is a substantial share of a bulk build, around 45% at 1M
/// points, split roughly evenly between the content check and `rtreecheck`. It
/// is not currently possible to switch off entirely: a bulk build writes the
/// RTree by hand, and verifying it is what makes that defensible. Whether that
/// remains the right default is a 1.0 question.
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
            fill_factor: DEFAULT_FILL_FACTOR,
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
    ///
    /// For [`crate::Layer::write_all`] this forces the path, not a rebuild: a
    /// write small relative to an already populated index still has its entries
    /// added to that index rather than the index rebuilt around them. Use
    /// [`crate::Layer::create_spatial_index_with`] to rebuild outright.
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

    /// Set the fraction of each node's capacity to fill, in `(0, 1]`.
    ///
    /// Values outside that range are clamped, and every node holds at least one
    /// entry regardless. See [`DEFAULT_FILL_FACTOR`] for the trade-off.
    #[must_use]
    pub fn with_fill_factor(mut self, fill_factor: f64) -> Self {
        self.fill_factor = fill_factor;
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
    /// The bulk build, gate passed.
    Bulk,
    /// The bulk build was attempted but its gate failed, so the triggered
    /// population was used as a fallback.
    TriggeredFallback,
}

/// A way for a test to make something go wrong during a bulk index build.
///
/// It is called on the index rows just after they are written and just before
/// [`gate`] checks them, which is the only moment at which either kind of
/// failure this exists to test can be introduced.
///
/// Outside tests this is always [`no_fault`], which does nothing at all. A test
/// passes either a function that corrupts the index that was just written, to
/// show that the gate rejects it and the triggered rebuild still produces a
/// correct one, or a function that returns an error, to show that the whole
/// build is rolled back.
pub(crate) type TestFault = fn(&Connection, &str) -> Result<()>;

/// The [`TestFault`] used outside tests: it does nothing.
pub(crate) fn no_fault(_: &Connection, _: &str) -> Result<()> {
    Ok(())
}

/// Writes a packed tree into the three shadow tables through cached prepared
/// statements.
///
/// Node and parent rows go straight out: the packer emits them in ascending
/// node order, which is also their primary-key order, so they append to the
/// B-tree. `%_rowid` rows are the exception. Its key is the feature id, but the
/// packer emits them in the tree's leaf order, which is Hilbert order and
/// therefore close to random with respect to feature id. Inserting a million of
/// those in that order is dominated by page splits: measured at 1M points it
/// cost 556 ms on uniformly spread data and 1.76 s on clustered data, against
/// 36 ms for the node rows. Buffering them and inserting in feature-id order
/// costs 16 bytes per entry and a sort, and brings both to ~265 ms, removing
/// the distribution sensitivity along with the cost.
struct ShadowTables<'c> {
    conn: &'c Connection,
    node_sql: String,
    rowid_sql: String,
    parent_sql: String,
    /// `(rowid, nodeno)` pairs, flushed in rowid order by [`Self::flush`].
    rowid_buffer: Vec<(i64, i64)>,
}

impl ShadowTables<'_> {
    /// Insert the buffered `%_rowid` rows in key order.
    fn flush(&mut self) -> Result<()> {
        self.rowid_buffer.sort_unstable_by_key(|&(rowid, _)| rowid);
        let mut stmt = self.conn.prepare_cached(&self.rowid_sql)?;
        for &(rowid, nodeno) in &self.rowid_buffer {
            stmt.execute(rusqlite::params![rowid, nodeno])?;
        }
        Ok(())
    }
}

impl NodeSink for ShadowTables<'_> {
    fn node(&mut self, nodeno: i64, blob: &[u8]) -> Result<()> {
        self.conn
            .prepare_cached(&self.node_sql)?
            .execute(rusqlite::params![nodeno, blob])?;
        Ok(())
    }

    fn rowid(&mut self, rowid: i64, nodeno: i64) -> Result<()> {
        self.rowid_buffer.push((rowid, nodeno));
        Ok(())
    }

    fn parent(&mut self, nodeno: i64, parentnode: i64) -> Result<()> {
        self.conn
            .prepare_cached(&self.parent_sql)?
            .execute(rusqlite::params![nodeno, parentnode])?;
        Ok(())
    }
}

/// The byte length the RTree module expects every node blob of `rtree` to have.
///
/// Read from the freshly created index's root node rather than re-derived from
/// `PRAGMA page_size`, because that is exactly what the module itself does when
/// it reopens an existing index: it takes the node size from `length(data)` of
/// node 1 and rejects any node that differs. Reading it keeps the two
/// definitions from drifting apart.
fn node_size(conn: &Connection, rtree: &str) -> Result<usize> {
    let size: i64 = conn.query_row(
        &format!(
            "SELECT length(data) FROM {} WHERE nodeno = 1",
            quote(&format!("{rtree}_node"))?
        ),
        [],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(size).unwrap_or(0))
}

/// Clear the three shadow tables and stream a freshly packed tree into them.
fn write_packed(
    conn: &Connection,
    rtree: &str,
    entries: &[(i64, [f64; 4])],
    node_size: usize,
    fill_factor: f64,
) -> Result<()> {
    let node_table = quote(&format!("{rtree}_node"))?;
    let rowid_table = quote(&format!("{rtree}_rowid"))?;
    let parent_table = quote(&format!("{rtree}_parent"))?;

    conn.execute_batch(&format!(
        "DELETE FROM {node_table}; DELETE FROM {rowid_table}; DELETE FROM {parent_table};"
    ))?;

    let mut sink = ShadowTables {
        conn,
        node_sql: format!("INSERT INTO {node_table} VALUES (?1, ?2)"),
        rowid_sql: format!("INSERT INTO {rowid_table} VALUES (?1, ?2)"),
        parent_sql: format!("INSERT INTO {parent_table} VALUES (?1, ?2)"),
        rowid_buffer: Vec::with_capacity(entries.len()),
    };
    packed::pack_into(entries, node_size, fill_factor, &mut sink)?;
    sink.flush()
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

/// The XY envelope to index for one geometry blob, or `None` if the row is not
/// indexable (empty under the same rule the `ST_*` functions apply).
///
/// This reproduces in a single parse what the previous SQL scan computed with
/// five `ST_*` calls per row. The blob is known non-NULL (the query's
/// `WHERE {c} NOT NULL` handles that). Emptiness is decided exactly as
/// `ST_IsEmpty` decides it (the header empty flag, else a body traversal) and
/// the bounds are taken from the header envelope when present and from the body
/// otherwise, exactly as `ST_MinX`/`ST_MaxX`/`ST_MinY`/`ST_MaxY` do. So the
/// returned set is identical to the one the old scan produced.
fn envelope_of(blob: &[u8]) -> std::result::Result<Option<[f64; 4]>, geopackage_core::Error> {
    // `ST_IsEmpty` traverses the body whenever the header flag is unset, so a
    // faithful set requires the same traversal here rather than trusting a
    // present header envelope. That traversal also yields the bounds for a blob
    // whose header carries no envelope, so one walk serves both purposes.
    let (bounds, _) = geometry::blob_envelope_and_empty(blob)?;
    Ok(bounds)
}

/// Accumulate `(fid, [min_x, max_x, min_y, max_y])` for every row whose geometry
/// is indexable, so the accumulated set is identical to what the triggered path
/// would index.
///
/// The envelope is derived by parsing each blob once in Rust (see
/// [`envelope_of`]). The previous form asked SQLite for
/// `ST_IsEmpty` plus the four `ST_Min/Max` functions per row: six user-function
/// dispatches, each re-fetching the blob and re-parsing the GPB header, and each
/// `ST_*` re-traversing the body for an envelope-less blob. Reading the blob
/// once collapses that to a single parse per row for the identical set. This
/// scan runs only on the standalone `create_spatial_index` path; the bulk
/// `write_all` path passes `precomputed` envelopes and never reaches here.
fn accumulate_envelopes(
    conn: &Connection,
    table: &str,
    geom: &str,
    pk: &str,
) -> Result<Vec<(i64, [f64; 4])>> {
    let (t, c, i) = (quote(table)?, quote(geom)?, quote(pk)?);
    let sql = format!("SELECT {i}, {c} FROM {t} WHERE {c} NOT NULL");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    // The row count is an upper bound on the entries kept: rows with a NULL or
    // empty geometry are skipped below. One count query is cheaper than the
    // repeated doubling it saves.
    let mut out = Vec::with_capacity(table_row_count(conn, table)?);
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        // Borrow the blob rather than `get::<Vec<u8>>`, so a million rows are
        // not each copied out of SQLite's page just to read a header.
        let blob = match row.get_ref(1)? {
            ValueRef::Blob(b) => b,
            // `WHERE {c} NOT NULL` rules out NULL; any other type in a geometry
            // column is malformed, which the old `ST_*` scan also surfaced as an
            // error rather than skipping.
            _ => {
                return Err(rusqlite::Error::InvalidColumnType(
                    1,
                    geom.to_string(),
                    rusqlite::types::Type::Blob,
                )
                .into());
            }
        };
        if let Some(bounds) = envelope_of(blob)? {
            out.push((id, bounds));
        }
    }
    Ok(out)
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
    mut expected: Vec<(i64, [f64; 4])>,
    structural_check: StructuralCheck,
) -> Result<bool> {
    let quoted = quote(rtree)?;
    let count: i64 = conn.query_row(&format!("SELECT count(*) FROM {quoted}"), [], |r| r.get(0))?;
    if usize::try_from(count).unwrap_or(usize::MAX) != expected.len() {
        return Ok(false);
    }

    // Sort and binary-search rather than building a `HashMap` of the whole
    // accumulated set. The map was a second copy of every entry held for the
    // duration of the scan, which at the row counts this path is built for is
    // hundreds of megabytes on top of the set already in hand.
    expected.sort_unstable_by_key(|&(id, _)| id);
    // A duplicate fid would make the bijection below ambiguous: two index rows
    // could match the same entry, or one could match twice. A primary key scan
    // cannot produce one, so treat it as a failed gate rather than trying to
    // resolve it.
    if expected
        .iter()
        .zip(expected.iter().skip(1))
        .any(|(a, b)| a.0 == b.0)
    {
        return Ok(false);
    }
    // One byte per entry, marking which have been matched, so a repeated id in
    // the index is caught the same way `HashMap::remove` returning `None`
    // caught it before.
    let mut matched = vec![false; expected.len()];
    let mut hits = 0usize;

    let mut stmt = conn.prepare(&format!("SELECT id, minx, maxx, miny, maxy FROM {quoted}"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let (s_min_x, s_max_x, s_min_y, s_max_y): (f64, f64, f64, f64) =
            (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
        let Ok(at) = expected.binary_search_by_key(&id, |&(id, _)| id) else {
            return Ok(false);
        };
        // `at` indexes both vectors, which are the same length, so neither
        // lookup can miss; the fallible forms keep the panicking index out.
        let (Some(seen), Some(&(_, [min_x, max_x, min_y, max_y]))) =
            (matched.get_mut(at), expected.get(at))
        else {
            return Ok(false);
        };
        if std::mem::replace(seen, true) {
            return Ok(false);
        }
        hits += 1;
        if !(s_min_x <= min_x && s_max_x >= max_x && s_min_y <= min_y && s_max_y >= max_y) {
            return Ok(false);
        }
    }
    if hits != expected.len() {
        return Ok(false);
    }

    // Structural check on top of the content gate above. `rtreecheck` walks the
    // index just built, cross-checking every entry against its parent's bounds
    // and the `%_rowid`/`%_parent` mappings against `%_node`, and reports `ok`
    // or a description of what is wrong. Its cost is proportional to the index;
    // `PRAGMA integrity_check` is O(database), so it is opt-in (issue #16).
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

/// Build (or rebuild) the RTree `rtree` for `table`/`geom` via the bulk
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
/// `fault` is [`no_fault`] outside tests.
///
/// This opens and commits its own transaction. A caller that already holds one,
/// and wants the build to be part of it, calls [`fill_index_in_transaction`]
/// instead.
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
    fault: TestFault,
    after: F,
) -> Result<BuildPath>
where
    F: FnOnce(&Connection) -> Result<()>,
{
    // Nothing in the build needs autocommit: the tree is constructed in memory
    // by `packed::pack` and written as ordinary rows, so unlike the previous
    // `ATTACH`ed scratch database this can all sit inside one transaction.
    let tx = conn.unchecked_transaction()?;
    let path = fill_index_in_transaction(
        &tx,
        table,
        geom,
        pk,
        rtree,
        options,
        precomputed,
        fault,
        after,
    )?;
    tx.commit()?;
    Ok(path)
}

/// [`fill_index`] without the transaction management: every statement runs on
/// `conn`, which the caller must already have inside a transaction, and nothing
/// is committed here.
///
/// The bulk `write_all` path uses this so the row inserts and the index build
/// commit together: a crash cannot then leave rows committed against an index
/// that was never rebuilt.
#[expect(
    clippy::too_many_arguments,
    reason = "internal build entry point threading the whole build context; a parameter struct would be used by these two call sites alone"
)]
pub(crate) fn fill_index_in_transaction<F>(
    conn: &Connection,
    table: &str,
    geom: &str,
    pk: &str,
    rtree: &str,
    options: BulkIndexOptions,
    precomputed: Option<Vec<(i64, [f64; 4])>>,
    fault: TestFault,
    after: F,
) -> Result<BuildPath>
where
    F: FnOnce(&Connection) -> Result<()>,
{
    let accumulated = match precomputed {
        Some(entries) => entries,
        None => accumulate_envelopes(conn, table, geom, pk)?,
    };

    let quoted_rtree = quote(rtree)?;
    let create_vtab = triggers::create_rtree_table_sql(table, geom)?;

    conn.execute_batch(&format!("DROP TABLE IF EXISTS {quoted_rtree}"))?;
    conn.execute_batch(&create_vtab)?;

    let node_size = node_size(conn, rtree)?;
    write_packed(conn, rtree, &accumulated, node_size, options.fill_factor)?;
    fault(conn, rtree)?;

    let path = if gate(conn, rtree, accumulated, options.structural_check)? {
        BuildPath::Bulk
    } else {
        // Discard the packed result and rebuild through the triggered
        // population, which cannot be affected by a bad packed tree.
        conn.execute_batch(&format!("DROP TABLE {quoted_rtree}"))?;
        conn.execute_batch(&create_vtab)?;
        conn.execute_batch(&triggers::populate_rtree_sql(table, geom, pk)?)?;
        BuildPath::TriggeredFallback
    };

    after(conn)?;
    Ok(path)
}
