# Transactions

SQLite has no nested transactions. A `BEGIN` issued while one is already open
fails with "cannot start a transaction within a transaction".

A caller can open a transaction of their own, through `GeoPackage::connection`
in Rust or `gpkg_begin` in the C ABI, and then use the ordinary write API. So
every write path in this library checks whether a transaction is already open
before it begins one.

## Inheriting a transaction

If a transaction is open, a write path runs its statements inside it. The
statements still commit or roll back together, but the caller's `COMMIT` or
`ROLLBACK` decides when.

This applies to every write path, from a single `FeatureWriter` insert to a
bulk `write_all` and the extent recording described in
[The layer extent](extent.md).

## What inheritance changes for the caller

**`commit` leaves the transaction open** when it was inherited. It still does
everything else a commit does, including flushing the `gpkg_contents`
`last_change` and bounding box, but the caller issues the durable commit.

**Dropping a writer does not roll back** when the transaction was inherited.
An error part-way through leaves partial work staged in the caller's
transaction, and the caller has to discard it.

**`write_all`'s `batch_size` no longer bounds transactions**, because every
batch belongs to the caller's transaction. The rows are written in the same
order, through the same statements.

A writer does not report whether its transaction was inherited. Only a caller
who opened a transaction can be in this situation, and that caller already
knows.

## Beginning without locking

`Layer::writer` opens a transaction without writing anything, and does not
take a lock. SQLite's `BEGIN DEFERRED` acquires nothing until the first
statement needs it, so the first failure a caller sees comes from the first
row, not from the call that returned the writer.

On a read-only connection, opening a writer succeeds and writing the first row
fails. Under contention, the wait for another connection's lock happens at the
first write. A successful call to open a writer does not show that the file is
writable.

## Reading and writing on one connection at once

A writer and a feature cursor share the connection, so a scan can drive its
own updates: read a row, recompute a column, write it back.

SQLite does not define what such a scan sees. Its isolation documentation
says that a `SELECT` on one connection has no isolation from writes on that
same connection, and that an application "can UPDATE the current row or any
prior row, though doing so might cause that row to reappear in a subsequent
`sqlite3_step()`". SQLite guarantees that the file will not be corrupted, but
not that the result set is stable, complete, or free of repeats.

In practice the scan is stable when all three of these are true:

- the cursor is a plain table scan, not one driven by an index;
- the columns written are not ones the scan's index reads;
- the primary key is not written, since changing a row's id moves it within a
  rowid scan.

Do not write a geometry during a bounding-box cursor. That cursor is driven by
a join against the RTree, and writing a geometry moves the row within that
index through the triggers, which can make the scan return rows it has already
returned. To recompute geometries, use two passes: collect the feature ids,
finish the scan, then write.

This is SQLite's documented behaviour for one connection. A second connection
avoids it, but has its own locking to manage.

## Getting the work done

- [How to copy features between files without decoding geometry](../how-to/copy-features.md)
  shows a long write committed in batches, and what changes when the caller
  owns the transaction.
- [`GeoPackage::connection`](https://docs.rs/geopackage/latest/geopackage/struct.GeoPackage.html#method.connection)
  returns the underlying connection, on which a caller can open a transaction.
