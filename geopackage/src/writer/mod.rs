//! Writing features and attribute rows: [`FeatureWriter`] and the batched
//! [`Layer::write_all`] helper.
//!
//! # Transaction shape
//!
//! [`Layer::writer`] returns a [`FeatureWriter`] that **owns its transaction**.
//! It opens it with rusqlite's `unchecked_transaction`, so it works on the
//! shared `&Connection` the read path already uses. Writes are staged in that
//! transaction. [`FeatureWriter::commit`] flushes the `gpkg_contents`
//! `last_change` and bounding box, then commits. Dropping a writer without
//! committing rolls the transaction back.
//!
//! rusqlite types never appear in the public API. Geometry is
//! `impl geo_traits::GeometryTrait<T = f64>`, and non-geometry values are the
//! crate's own value types. The per-row entry points take borrowed
//! [`crate::ValueRef`]s, so a row read from one layer can be bound into another
//! without copying its text and blob cells. [`NewFeature`] stores owned
//! [`Value`]s, because [`Layer::write_all`] consumes an iterator whose rows
//! have to outlive any single call.
//!
//! The writer owns its transaction, rather than taking a transaction object
//! from the caller, for two reasons. It keeps `rusqlite::Transaction` out of
//! the public API, and it lets the writer maintain the running bounding-box
//! fold and `last_change` at one commit point. If you want to drive your own
//! transaction, the raw connection ([`crate::GeoPackage::connection`]) is still
//! available.
//!
//! ## When the caller has already begun a transaction
//!
//! SQLite does not nest transactions. A writer opened while a transaction is
//! already open on the connection joins it instead
//! ([`crate::transaction::WriteTransaction`] documents the reasoning). This has
//! three consequences, and they apply to every write path in the crate, not
//! just this one:
//!
//! - [`FeatureWriter::commit`] stages the `gpkg_contents` flush and returns
//!   success without committing. The caller issues the durable commit.
//! - Dropping a writer rolls nothing back, so an error part-way through leaves
//!   what preceded it staged for the caller to discard.
//! - [`Layer::write_all`]'s `batch_size` stops bounding transactions, because
//!   every batch belongs to the caller's transaction. Nothing else changes: the
//!   rows are written in the same order and the same statements are used.
//!
//! A writer deliberately gives no way to tell which case applies. A caller who
//! opened a transaction knows they did, and a caller who didn't can't run into
//! this behaviour.
//!
//! # Updating a layer while a cursor over it is stepping
//!
//! A writer and a [`crate::FeatureCursor`] share the connection, so a scan can
//! drive its own updates: read a row, recompute a column, write it back. That
//! is what [`FeatureWriter::update_columns`] is for.
//!
//! SQLite does not define what such a scan sees. Its isolation documentation
//! says that a `SELECT` on one connection has no isolation from writes on that
//! same connection, and that an application "can UPDATE the current row or any
//! prior row, though doing so might cause that row to reappear in a subsequent
//! `sqlite3_step()`". SQLite promises only that the file will not be harmed. It
//! does not promise that the result set is stable, complete, or free of
//! repeats.
//!
//! The scan is stable in practice when all three of these hold:
//!
//! - the cursor is a plain table scan ([`crate::Layer::cursor`]) rather than
//!   one driven by an index;
//! - the columns written are not ones the scan's index reads;
//! - the primary key is not written, since moving a row's id moves it within a
//!   rowid scan.
//!
//! In particular, don't write a geometry during a [`crate::Layer::cursor_in`]
//! scan. That cursor is driven by a join against the RTree. Writing a geometry
//! moves the row within that index through the triggers, and that is what makes
//! a scan return rows it has already returned. To recompute geometries, use two
//! passes: collect the feature ids, finish the scan, then write.
//!
//! None of this is specific to this crate, and none of it is a bug this crate
//! can fix. It is SQLite's stated contract for one connection reading and
//! writing at once.
//!
//! # Bounding box and `last_change`
//!
//! The writer seeds a bounding-box fold from the existing `gpkg_contents` row
//! and unions each written geometry's XY envelope into it. This is a cheap
//! running fold, never a rescan. Deletes do not shrink the box, because an
//! over-estimate is spec-legal and shrinking would need a rescan. On commit,
//! the writer writes back a non-empty fold and sets `last_change` to the strict
//! 1.4 datetime form using SQLite's `strftime`, matching the normative column
//! default.
//!
//! The writer writes the fold back only when it can guarantee the fold covers
//! the layer:
//!
//! - If the fold was seeded from a usable recorded box, growing it keeps it a
//!   valid cover.
//! - If there was no usable box and the table was empty, the geometries written
//!   are the whole content, so the fold is exact.
//! - If there was no usable box but the table already contained rows, the fold
//!   covers only what this writer wrote. Recording it would replace an accurate
//!   "unknown" with a box that excludes every pre-existing row, and readers
//!   trust a well-ordered extent indefinitely. In this case the writer leaves
//!   the extent alone, and [`Layer::recompute_extent`] fixes it.
//!
//! See [`crate::extent`] for the full reasoning.
//!
//! # Envelopes and Z/M
//!
//! Every written geometry gets a GPB envelope (XY, or XYZ when it has Z). A
//! reader, and the rtree triggers that ask for four bounds a row, never have to
//! decode the WKB body to get them. Encoding is delegated to
//! [`geopackage_core::geometry::encode_gpb`]. Before encoding, the writer
//! validates a geometry's `z`/`m` presence against the column's [`ZmFlag`], so
//! a violation is a typed [`Error::ZmViolation`] rather than a malformed row.
//!
//! # Spatial indexes
//!
//! Individual `insert`/`update`/`delete` calls, and the per-batch
//! [`Layer::write_all`] path, go through ordinary SQL. If a table already has
//! the rtree triggers, those triggers maintain its index (the `ST_*` functions
//! are registered on every connection).
//!
//! [`Layer::write_all`] also has a bulk path, which it takes when it writes a
//! large batch into an indexed layer. It drops the triggers, inserts the rows
//! without per-row index maintenance, brings the index up to date in one
//! operation, and reinstalls the triggers. How it brings the index up to date
//! depends on the size of the write relative to the size of the index. It
//! chooses once the rows are written and both counts are known. A write large
//! enough to be worth it rebuilds the index outright (see [`crate::bulk`]), and
//! a smaller one adds the new entries to the existing index instead.
//! [`BulkIndexOptions`], passed to [`Layer::write_all_with`], controls the
//! threshold at which the bulk path engages and can force it either way.
//!
//! # Atomicity of the bulk path
//!
//! The bulk `write_all` is a single transaction. Dropping the triggers, every
//! row insert, the `gpkg_contents` flush, the index work at the end (rebuild or
//! append), and reinstalling the triggers all commit together. A crash or an
//! error at any point rolls the whole thing back to the state before the call,
//! so the rows can never be committed against an index that was not brought up
//! to date with them.
//! The exception is a call inside a transaction you opened. A failed call then
//! leaves its partial work, dropped triggers included, staged in your
//! transaction for you to roll back (see [`crate::transaction`]).
//!
//! This was not always so. The rebuild used to run in its own transaction
//! because it built the index in an `ATTACH`ed scratch database, and `ATTACH`
//! requires autocommit. That left a window in which a crash committed the rows
//! but not the index. Building the tree directly ([`crate::packed`]) removed
//! the `ATTACH`, and with it the window. [`Layer::spatial_index_status`] and
//! [`Layer::repair_spatial_index`] still exist and still recover a
//! [`crate::SpatialIndexStatus::Stale`] index, since a file can arrive from
//! anywhere, but this path produces one only if you commit your own
//! transaction after a failed call inside it.
//!
//! [`Layer::write_all`]: crate::Layer::write_all
//! [`Layer::writer`]: crate::Layer::writer
//! [`Layer::write_all_with`]: crate::Layer::write_all_with
//! [`Layer::recompute_extent`]: crate::Layer::recompute_extent
//! [`Layer::spatial_index_status`]: crate::Layer::spatial_index_status
//! [`Layer::repair_spatial_index`]: crate::Layer::repair_spatial_index
//! [`Value`]: crate::Value
//! [`BulkIndexOptions`]: crate::BulkIndexOptions
//! [`ZmFlag`]: geopackage_core::ZmFlag
//! [`Error::ZmViolation`]: crate::Error::ZmViolation
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
