//! Bulk RTree builds: construct the index in memory and write it directly.
//!
//! SQLite's RTree module has no bulk-load entry point. Populating an index
//! through the spec's `INSERT INTO rtree SELECT` statement or through the
//! per-row triggers inserts one row at a time and pays the node-splitting cost
//! for every row. [gdal#7614](https://github.com/OSGeo/gdal/issues/7614) makes
//! the same observation from GDAL's side. This module originally built the
//! RTree in an `ATTACH`ed scratch database and copied its shadow tables into
//! the target. Measurement showed that the scratch build was then the dominant
//! cost, because it still inserted every entry through the module one row at a
//! time.
//!
//! The tree is now built directly. [`crate::packed`] lays out the
//! `rtree_%_node` / `_rowid` / `_parent` contents in memory from the entry set,
//! and this module writes them as ordinary rows. The build uses no RTree module
//! logic, no `ST_*` calls, no triggers and no scratch database.
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
//! Every bulk build must pass a **gate** before it is trusted (see [`gate`]).
//! The written index must contain exactly the accumulated `(fid, envelope)`
//! set, checked by row count and a per-row containment check. It must also pass
//! a structural check ([`StructuralCheck`]): `rtreecheck()` on the index by
//! default, or optionally a whole-database `PRAGMA integrity_check`. If either
//! check fails, [`fill_index`] falls back to the triggered population
//! statement, so a failed gate never yields a corrupt or stale index, only a
//! slower build.
//!
//! **The gate is about 45% of the build.** Profiled over 1M points, it takes
//! ~745 ms of a ~1593 ms build. Roughly half of that is the bijection scan,
//! which reads every entry back out of the index, and half is `rtreecheck`,
//! which walks the whole tree. GDAL's builder runs no equivalent. Without the
//! gate this crate would be comfortably faster than GDAL, rather than level
//! with it (see `roadmap/benchmarks/2026-07-24-gdal-like-for-like.md`).
//!
//! The cost is deliberate and stays for now. This module writes the tree by
//! hand into an on-disk format that SQLite doesn't document as an interface, so
//! the result is checked against SQLite's own checker and against the input
//! before anything relies on it. Whether to remove the gate or turn it off by
//! default is a question for 1.0, once the packer has enough history to justify
//! that trust. It shouldn't change while the format handling is this new. Until
//! then, roughly half of a bulk index build is spent on checking, and it is
//! kept for confidence, not speed.
//!
//! The entry set itself comes either from an `ST_*` scan of the table or, when
//! the caller can prove it accounts for every indexable row, from envelopes
//! computed while encoding the geometries (see [`fill_index`]'s `precomputed`).

use geopackage_core::geometry;
use geopackage_core::ident::quote;
use geopackage_core::triggers;
use rusqlite::Connection;
use rusqlite::config::DbConfig;
use rusqlite::types::ValueRef;

use crate::Result;
use crate::packed::{self, NodeSink};
use crate::transaction::WriteTransaction;

/// Default candidate-row count at or above which
/// [`crate::Layer::create_spatial_index`] and [`crate::Layer::write_all`] choose
/// the bulk shadow-table build over the per-row triggered build.
///
/// Below this, the fixed cost of building and writing the tree, plus the gate,
/// outweighs the saving. Above it, the bulk build wins. Override it with
/// [`BulkIndexOptions`].
pub const DEFAULT_BULK_THRESHOLD: usize = 10_000;

/// Default fraction of each RTree node's capacity used by the bulk build.
///
/// Packing nodes full gives the smallest tree, the shallowest descent and the
/// best queries, which suits a bulk load. However, a full node has no room for
/// a later insert, so the first append into it splits immediately. Appends
/// after a bulk load go through the triggers, the per-row path this build
/// exists to avoid, so the default favours the load and the queries. Lower it
/// with [`BulkIndexOptions::with_fill_factor`] if you will append heavily to a
/// freshly built index.
pub const DEFAULT_FILL_FACTOR: f64 = 1.0;

/// Tuning for the RTree bulk-build path.
///
/// Pass it to [`crate::Layer::create_spatial_index_with`]. It also applies to
/// the bulk `write_all` path. The default threshold is
/// [`DEFAULT_BULK_THRESHOLD`]. [`Self::always_bulk`] and [`Self::never_bulk`]
/// force one path or the other, mainly for tests and benchmarking.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct BulkIndexOptions {
    /// Candidate-row count at or above which the bulk build is used. `0` always
    /// uses the bulk path. [`usize::MAX`] always uses the triggered path.
    ///
    /// For [`crate::Layer::write_all`] this also bounds a buffer. If a source
    /// does not advertise its length, up to this many rows are held back to
    /// find out whether it reaches the threshold. Raising the threshold raises
    /// that bound, so a very large value paired with a lazy iterator can hold a
    /// correspondingly large number of rows in memory. `usize::MAX` buffers
    /// nothing, because it disables the path outright.
    pub bulk_threshold: usize,
    /// How much of the written index is checked before it is trusted.
    /// Defaults to [`BulkVerification::None`].
    pub verification: BulkVerification,
    /// Fraction of each RTree node's capacity to fill when packing, in
    /// `(0, 1]`. Defaults to [`DEFAULT_FILL_FACTOR`].
    pub fill_factor: f64,
}

/// How much of a bulk-built RTree is checked before it is trusted.
///
/// Every level above [`Self::None`] also enables the automatic fallback. A
/// build that fails its check is discarded and rebuilt through the triggered
/// path, so the result is correct either way and only time is lost. **With
/// [`Self::None`] nothing is checked, so nothing can fail and fall back.**
///
/// # Why the default changed
///
/// Through 0.5 the default read the whole index back and ran `rtreecheck`
/// over it. That costs about **45% of the build**: roughly 745 ms of a 1593 ms
/// build at one million points, split about evenly between the two checks.
/// The cost was worth paying while `crate::packed` was new, because it writes
/// an RTree by hand into a format SQLite does not document as an interface.
///
/// The packer now has enough history for the check to be opt-in rather than
/// paid by everyone. Turn it back on if you are writing files that matter or
/// bisecting a suspected index problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BulkVerification {
    /// Trust the packer. The default, and the fastest.
    ///
    /// Nothing is read back, so a build cannot fail its check and cannot fall
    /// back to the triggered path. You can still call
    /// `crate::Layer::audit_spatial_index` afterwards if you need the answer
    /// later rather than on every write.
    #[default]
    None,
    /// Read every entry back and check it against the envelopes accumulated
    /// while writing: one index row per indexable row, a bijection on `id`, and
    /// each stored bound containing the true envelope.
    ///
    /// This is the cheaper half of the pre-0.6 default. It catches a wrong or
    /// missing entry, which is what a query would notice.
    Contents,
    /// [`Self::Contents`], plus `rtreecheck` over the tree, which cross-checks
    /// every entry against its parent's bounds and the `%_rowid`/`%_parent`
    /// mappings against `%_node`.
    ///
    /// This was the pre-0.6 default. It catches a structurally malformed tree
    /// that the content check would pass, a failure particular to writing the
    /// shadow tables by hand.
    Structure,
    /// [`Self::Structure`], plus a whole-database `PRAGMA integrity_check`.
    ///
    /// This costs O(database) rather than O(index), so on a large file it
    /// dominates everything else. A benign pre-existing problem anywhere in the
    /// file also forces the triggered fallback, which is still correct. Use it
    /// to validate a file of unknown provenance.
    Database,
}

impl Default for BulkIndexOptions {
    fn default() -> Self {
        Self {
            bulk_threshold: DEFAULT_BULK_THRESHOLD,
            verification: BulkVerification::default(),
            fill_factor: DEFAULT_FILL_FACTOR,
        }
    }
}

impl BulkIndexOptions {
    /// Creates options with an explicit bulk threshold.
    pub fn with_threshold(bulk_threshold: usize) -> Self {
        Self {
            bulk_threshold,
            ..Self::default()
        }
    }

    /// Always takes the bulk path (threshold `0`).
    ///
    /// For [`crate::Layer::write_all`] this forces the path, not a rebuild. A
    /// write that is small relative to an already populated index still adds
    /// its entries to that index, rather than rebuilding the index around them.
    /// Use [`crate::Layer::create_spatial_index_with`] to rebuild outright.
    pub fn always_bulk() -> Self {
        Self::with_threshold(0)
    }

    /// Never takes the bulk path (threshold [`usize::MAX`]).
    pub fn never_bulk() -> Self {
        Self::with_threshold(usize::MAX)
    }

    /// Sets the structural check the gate runs after the copy.
    #[must_use]
    pub fn with_verification(mut self, verification: BulkVerification) -> Self {
        self.verification = verification;
        self
    }

    /// Sets the fraction of each node's capacity to fill, in `(0, 1]`.
    ///
    /// Values outside that range are clamped, and every node contains at least
    /// one entry regardless. See [`DEFAULT_FILL_FACTOR`] for the trade-off.
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
/// It runs on the index rows just after they are written and just before
/// [`gate`] checks them. That is the only point at which either kind of failure
/// it tests can be introduced.
///
/// Outside tests this is always [`no_fault`], which does nothing. A test passes
/// one of two kinds of function. One corrupts the index that was just written,
/// to show that the gate rejects it and the triggered rebuild still produces a
/// correct one. The other returns an error, to show that the whole build is
/// rolled back.
pub(crate) type TestFault = fn(&Connection, &str) -> Result<()>;

/// The [`TestFault`] used outside tests: it does nothing.
pub(crate) fn no_fault(_: &Connection, _: &str) -> Result<()> {
    Ok(())
}

/// Writes a packed tree into the three shadow tables through cached prepared
/// statements.
///
/// Node and parent rows are written immediately. The packer emits them in
/// ascending node order, which is also their primary-key order, so they append
/// to the B-tree. `%_rowid` rows are the exception. Their key is the feature
/// id, but the packer emits them in the tree's leaf order. That is Hilbert
/// order, and so close to random with respect to feature id. Page splits
/// dominate inserting a million rows in that order. Measured at 1M points, it
/// cost 556 ms on uniformly spread data and 1.76 s on clustered data, against
/// 36 ms for the node rows. Buffering the rows and inserting them in feature-id
/// order costs 16 bytes per entry and a sort. It brings both cases to ~265 ms,
/// and removes the sensitivity to data distribution along with the cost.
struct ShadowTables<'c> {
    conn: &'c Connection,
    node_sql: String,
    rowid_sql: String,
    parent_sql: String,
    /// `(rowid, nodeno)` pairs, flushed in rowid order by [`Self::flush`].
    rowid_buffer: Vec<(i64, i64)>,
}

impl ShadowTables<'_> {
    /// Inserts the buffered `%_rowid` rows in key order.
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
/// This is read from the freshly created index's root node rather than
/// re-derived from `PRAGMA page_size`. The module itself does the same when it
/// reopens an existing index: it takes the node size from `length(data)` of
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

/// Lifts SQLite's defensive flag for the duration of the direct shadow-table
/// writes, restoring the value found on drop.
///
/// Stock SQLite ships with the flag off, and the packed build assumes that.
/// [`write_packed`] inserts into `rtree_*_node` / `_rowid` / `_parent`
/// directly, which defensive mode rejects with "table ... may not be
/// modified". Apple's system SQLite turns the flag on by default, so a
/// system-linked build on macOS cannot bulk-build an index without lifting
/// it. The flag is lifted only for the packed write, not cleared at open, so
/// a hardened connection keeps its hardening everywhere else.
struct DefensiveOff<'conn> {
    conn: &'conn Connection,
    /// The flag as found. Only a lifted flag is restored.
    was_on: bool,
}

impl<'conn> DefensiveOff<'conn> {
    fn lift(conn: &'conn Connection) -> Result<Self> {
        let was_on = conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)?;
        if was_on {
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, false)?;
        }
        Ok(Self { conn, was_on })
    }
}

impl Drop for DefensiveOff<'_> {
    fn drop(&mut self) {
        if self.was_on
            && self
                .conn
                .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
                .is_err()
        {
            // Restoration fails only on a connection that is already unusable,
            // where the flag no longer guards anything.
        }
    }
}

/// Clears the three shadow tables and streams a freshly packed tree into
/// them.
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

/// Returns the number of rows in `table`, a cheap proxy for the path decision
/// that makes no `ST_*` calls.
pub(crate) fn table_row_count(conn: &Connection, table: &str) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {}", quote(table)?),
        [],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// Returns the XY envelope to index for one geometry blob, or `None` if the
/// row is not indexable (empty under the same rule the `ST_*` functions
/// apply).
///
/// It computes in a single parse what the previous SQL scan computed with five
/// `ST_*` calls per row. The blob is known to be non-NULL, because the query's
/// `WHERE {c} NOT NULL` excludes NULLs. Emptiness is decided exactly as
/// `ST_IsEmpty` decides it: by the header empty flag, or else a body traversal.
/// The bounds come from the header envelope when present and from the body
/// otherwise, exactly as `ST_MinX`/`ST_MaxX`/`ST_MinY`/`ST_MaxY` do. The
/// returned set is therefore identical to the one the old scan produced.
fn envelope_of(blob: &[u8]) -> std::result::Result<Option<[f64; 4]>, geopackage_core::Error> {
    // `ST_IsEmpty` traverses the body whenever the header flag is unset, so a
    // faithful set requires the same traversal here rather than trusting a
    // present header envelope. That traversal also yields the bounds for a blob
    // whose header has no envelope, so one walk serves both purposes.
    let (bounds, _) = geometry::blob_envelope_and_empty(blob)?;
    Ok(bounds)
}

/// Accumulates `(fid, [min_x, max_x, min_y, max_y])` for every row whose
/// geometry is indexable, so the set is identical to what the triggered path
/// would index.
///
/// Each envelope comes from parsing the blob once in Rust (see
/// [`envelope_of`]). The previous version asked SQLite for `ST_IsEmpty` plus
/// the four `ST_Min/Max` functions per row. That was six user-function
/// dispatches, each re-fetching the blob and re-parsing the GPB header, and
/// each `ST_*` call re-traversed the body of a blob without an envelope.
/// Reading the blob once reduces that to a single parse per row, for the same
/// set. This scan runs only on the standalone `create_spatial_index` path. The
/// bulk `write_all` path passes `precomputed` envelopes and never reaches it.
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

/// Gates a freshly copied RTree against the accumulated `(fid, envelope)`
/// set.
///
/// Passes only when the index contains exactly one row per accumulated entry
/// (checked by row count and a bijection on `id`), each stored bound
/// conservatively contains the true envelope, and `PRAGMA integrity_check`
/// reports `ok`. The RTree stores `f32` bounds, with minima rounded down and
/// maxima rounded up, so containment, not equality, is the correct relation.
fn gate(
    conn: &Connection,
    rtree: &str,
    mut expected: Vec<(i64, [f64; 4])>,
    verification: BulkVerification,
) -> Result<bool> {
    // Nothing to check, so nothing can fail and nothing falls back.
    if verification == BulkVerification::None {
        return Ok(true);
    }
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

    if verification == BulkVerification::Contents {
        return Ok(true);
    }

    // Structural check on top of the content gate above. `rtreecheck` walks the
    // index just built, cross-checking every entry against its parent's bounds
    // and the `%_rowid`/`%_parent` mappings against `%_node`, and reports `ok`
    // or a description of what is wrong. Its cost is proportional to the index;
    // `PRAGMA integrity_check` is O(database), so it is a level further up
    // (issue #16).
    let rtree_report: String = conn.query_row("SELECT rtreecheck(?1)", [rtree], |r| r.get(0))?;
    if rtree_report != "ok" {
        return Ok(false);
    }

    if verification == BulkVerification::Database {
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        return Ok(integrity == "ok");
    }
    Ok(true)
}

/// Builds (or rebuilds) the RTree `rtree` for `table`/`geom` via the bulk
/// shadow-table copy, gated with automatic fallback to the triggered
/// population.
///
/// On entry the virtual table may or may not exist, and the RTree triggers
/// must **not** be installed. For a rebuild, the caller drops them first. This
/// function (re)creates the virtual table empty, fills it, and runs `after`
/// inside the same transaction. The caller uses `after` to install the trigger
/// set and any `gpkg_extensions` row, so the whole operation commits atomically.
///
/// `precomputed` supplies the `(fid, envelope)` entry set directly instead of
/// deriving it from an `ST_*` scan of the table. Pass `Some` only when the
/// caller can prove it has an entry for every indexable row. The bulk
/// `write_all` path passes the envelopes it computed while encoding, and only
/// when the table was empty before the write. An incomplete set would build an
/// index that is missing rows. The gate cannot detect this, because it checks
/// the index against the same set. `None` scans the table, which is always
/// sound.
///
/// `fault` is [`no_fault`] outside tests.
///
/// This opens and commits its own transaction, or joins one already open on
/// `conn` and leaves the commit to whoever began it. A caller managing its own
/// transaction calls [`fill_index_in_transaction`] instead, which does no
/// transaction management at all.
#[expect(
    clippy::too_many_arguments,
    reason = "internal build entry point threading the whole build context; a parameter struct would be used by these two call sites alone"
)]
#[hotpath::measure(label = "bulk::fill_index")]
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
    let tx = WriteTransaction::begin(conn)?;
    let path = fill_index_in_transaction(
        conn,
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

/// [`fill_index`] without the transaction management. Every statement runs on
/// `conn`, which the caller must already have inside a transaction, and nothing
/// is committed here.
///
/// The bulk `write_all` path uses this so that the row inserts and the index
/// build commit together. A crash then cannot leave rows committed against an
/// index that was never rebuilt.
#[expect(
    clippy::too_many_arguments,
    reason = "internal build entry point threading the whole build context; a parameter struct would be used by these two call sites alone"
)]
#[hotpath::measure(label = "bulk::fill_index_in_transaction")]
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
    {
        let _defensive = DefensiveOff::lift(conn)?;
        write_packed(conn, rtree, &accumulated, node_size, options.fill_factor)?;
    }
    fault(conn, rtree)?;

    let path = if gate(conn, rtree, accumulated, options.verification)? {
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
