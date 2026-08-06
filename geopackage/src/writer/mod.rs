//! The feature/attribute write path: [`FeatureWriter`] and the batched
//! [`Layer::write_all`] helper.
//!
//! # Transaction shape
//!
//! [`Layer::writer`] returns a [`FeatureWriter`] that **owns its transaction**
//! (opened with rusqlite's `unchecked_transaction`, so it works on the shared
//! `&Connection` the read path already uses). Writes stage into that
//! transaction; [`FeatureWriter::commit`] flushes the `gpkg_contents`
//! `last_change` and bounding box, then commits. Dropping a writer without
//! committing rolls the transaction back. rusqlite types never appear in the
//! public API: geometry is `impl geo_traits::GeometryTrait<T = f64>` and
//! non-geometry values are the crate's own value types. The per-row entry
//! points take borrowed [`crate::ValueRef`]s, so a row read from one layer
//! binds into another without its text and blob cells being copied;
//! [`NewFeature`] stores owned [`Value`]s, because [`Layer::write_all`]
//! consumes an iterator whose rows have to outlive any single call.
//!
//! An owned transaction (rather than a caller-passed transaction object) keeps
//! the escape-hatch `rusqlite::Transaction` out of the public surface and lets
//! the writer maintain the running bounding-box fold and `last_change` at one
//! commit point. The raw connection ([`crate::GeoPackage::connection`]) remains
//! available for callers driving their own transaction.
//!
//! ## When the caller has already begun one
//!
//! SQLite does not nest transactions, so a writer opened while one is already
//! open on the connection joins it instead
//! ([`crate::transaction::WriteTransaction`], which documents the reasoning).
//! Three things follow, and they apply to every write path in the crate rather
//! than only to this one:
//!
//! - [`FeatureWriter::commit`] stages the `gpkg_contents` flush and returns
//!   success without committing. The caller issues the durable commit.
//! - Dropping a writer rolls nothing back, so an error part-way through leaves
//!   what preceded it staged for the caller to discard.
//! - [`Layer::write_all`]'s `batch_size` stops bounding transactions, because
//!   every batch belongs to the caller's transaction. It still bounds nothing
//!   else: the
//!   rows are written in the same order and the same statements are used.
//!
//! None of this is detectable from a writer, and deliberately so. A caller who
//! opened a transaction knows they did; one who did not cannot reach this
//! behaviour.
//!
//! # Updating a layer while a cursor over it is stepping
//!
//! A writer and a [`crate::FeatureCursor`] share the connection, so a scan can
//! drive its own updates: read a row, recompute a column, write it back. That
//! is what [`FeatureWriter::update_columns`] is for.
//!
//! SQLite does not define what such a scan sees. Its isolation documentation is
//! explicit that a `SELECT` on one connection has no isolation from writes on
//! that same connection, and that an application "can UPDATE the current row or
//! any prior row, though doing so might cause that row to reappear in a
//! subsequent `sqlite3_step()`". The safety it does promise is only that the
//! file will not be harmed; the result set is not promised to be stable,
//! complete, or free of repeats.
//!
//! The scan is stable in practice when all three of these hold:
//!
//! - the cursor is a plain table scan ([`crate::Layer::cursor`]) rather than one
//!   driven by an index;
//! - the columns written are not ones the scan's index reads;
//! - the primary key is not written, since moving a row's id moves it within a
//!   rowid scan.
//!
//! The case to avoid is writing a geometry during a
//! [`crate::Layer::cursor_in`] scan. That cursor is driven by a join against
//! the RTree, and writing a geometry moves the row inside that index through
//! the triggers, which is the shape that makes a scan return rows it has
//! already returned. Recomputing geometries is better done in two passes:
//! collect the feature ids, finish the scan, then write.
//!
//! None of this is specific to this crate, and none of it is a bug that can be
//! fixed here: it is SQLite's stated contract for one connection reading and
//! writing at once.
//!
//! # Bounding box and `last_change`
//!
//! The writer seeds a bounding-box fold from the existing `gpkg_contents` row
//! and unions each written geometry's XY envelope into it (a cheap running
//! fold, never a rescan). Deletes do not shrink the box: an over-estimate is
//! spec-legal, and shrinking would need a rescan. On commit, a non-empty fold
//! is written back and `last_change` is refreshed to the strict 1.4 datetime
//! form via SQLite's `strftime` (matching the normative column default).
//!
//! The fold is written back only when the writer can guarantee it covers the
//! layer. Seeded from a usable recorded box, growing it keeps it a valid
//! cover. Starting from no usable box over an empty table, the geometries
//! written are the whole content and the fold is exact. But starting from no
//! usable box over a table that already contains rows, the fold covers only
//! what this writer wrote, and recording it would replace an accurate
//! "unknown" with a box that excludes every pre-existing row. Readers believe a well-ordered extent indefinitely,
//! so that case leaves the extent alone; [`Layer::recompute_extent`] is how it
//! gets fixed. See [`crate::extent`] for the reasoning in full.
//!
//! # Envelopes and Z/M
//!
//! Every written geometry gets a GPB envelope (XY, or XYZ when it has Z),
//! so a reader, and the rtree triggers that ask for four bounds a row, never
//! have to decode the WKB body to get them; encoding is delegated to
//! [`geopackage_core::geometry::encode_gpb`]. A geometry's `z`/`m` presence is
//! validated against the column's [`ZmFlag`] before encoding, so a violation
//! is a typed [`Error::ZmViolation`] rather than a malformed row.
//!
//! # Spatial indexes
//!
//! Individual `insert`/`update`/`delete` calls, and the per-batch
//! [`Layer::write_all`] path, go through ordinary SQL, so a table that already
//! has the rtree triggers has its index maintained by those triggers (the
//! `ST_*` functions are registered on every connection).
//!
//! [`Layer::write_all`] additionally takes the bulk path when it writes a large
//! batch into an indexed layer: it drops the triggers, inserts the rows without
//! per-row index maintenance, brings the index up to date in one operation, and
//! reinstalls the triggers. How the index is brought up to date depends on the
//! size of the write against the size of the index, and is chosen once the rows
//! are written and both counts are known: a write large enough to be worth it
//! rebuilds the index outright (see [`crate::bulk`]), and a smaller one adds the
//! new entries to the existing index instead. The threshold at which the whole
//! path engages, and forcing it either way, are controlled by
//! [`BulkIndexOptions`] via [`Layer::write_all_with`].
//!
//! # Atomicity of the bulk path
//!
//! The bulk `write_all` is a single transaction: dropping the triggers, every
//! row insert, the `gpkg_contents` flush, the index work at the end (rebuild or
//! append), and reinstalling the triggers all commit together. A crash or an
//! error at any point rolls the whole thing back to the state before the call,
//! so the rows can never be committed against an index that was not brought up
//! to date with them.
//!
//! This was not always so. The rebuild used to run in its own transaction
//! because it built the index in an `ATTACH`ed scratch database and `ATTACH`
//! requires autocommit, which left a window where a crash committed the rows but
//! not the index. Building the tree directly ([`crate::packed`]) removed the
//! `ATTACH` and with it the window. [`Layer::spatial_index_status`] and
//! [`Layer::repair_spatial_index`] still exist and still recover a
//! [`crate::SpatialIndexStatus::Stale`] index, since a file can arrive from
//! anywhere, but this path no longer produces one.
mod constraints;
mod feature_writer;
mod row;
mod write_all;

pub use feature_writer::FeatureWriter;
pub use row::NewFeature;
// Re-exported for the Arrow write path; `write_all` names the trait through
// `super::row` directly.
#[cfg(feature = "arrow")]
pub(crate) use row::WritableRow;
