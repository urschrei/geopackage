# Transactions, and who commits

SQLite has no nested transactions. A `BEGIN` issued while one is already open
fails with "cannot start a transaction within a transaction". This shapes how
every write path in this library starts its work. You should understand it
before you open your own transaction on the underlying connection.

## The original problem

Originally, every write path here issued a `BEGIN` unconditionally. That works
while the library is the only thing driving the connection. It fails as soon
as a caller uses the escape hatch.

`GeoPackage::connection` returns the underlying rusqlite connection, because
SQLite is the query engine and anything the API does not cover should be a
query away. A caller who began a transaction on that connection and then used
the ordinary API got the nesting error instead of their writes. The C ABI had
a worse version of the problem. It could not offer `gpkg_begin` and
`gpkg_commit` at all, because the sequence a C consumer would reasonably write
could not work.

## Inheriting rather than nesting

Now a write path checks whether a transaction is already open. If one is, it
runs its statements inside it.

Atomicity is unchanged. The statements are still grouped, they still commit
together, and a failure still rolls them back together. What changes is who
decides when that happens: whoever issued the `BEGIN`.

Every write path in the library behaves this way, from a single
`FeatureWriter` insert to a bulk `write_all` and the extent recording
described in [the extent chapter](extent.md).

## Why not savepoints

Savepoints are the other way to get nesting out of SQLite. They were
considered and rejected.

`rusqlite::Connection::savepoint` needs `&mut Connection`, and this library has
only `&Connection`, because the read path shares it. Savepoints would
therefore have to be driven as raw `SAVEPOINT` and `RELEASE` text. That is
more machinery, for a finer rollback granularity that nobody has asked for.
The question is not closed. If a need arises, the transaction type is the one
place that would have to change.

## What inheritance costs the caller

These consequences apply to every write path, not only the feature writer.

**Commit does not commit** when the transaction was inherited. It still does
everything else a commit does, including flushing the `gpkg_contents`
`last_change` and bounding box, but the caller issues the durable commit.

**Dropping a writer does not roll back** when the transaction was inherited.
An error part-way through leaves partial work staged in the caller's
transaction, and the caller has to discard it.

**`write_all`'s `batch_size` stops bounding transactions**, because every batch
belongs to the caller's transaction. It bounds nothing else either: the rows
are written in the same order, through the same statements.

A writer does not expose whether its transaction was inherited, and this is
deliberate. A caller who opened a transaction knows they did. A caller who did
not cannot get this behaviour at all.

## Why the writer owns a transaction rather than taking one

The writer could instead accept a `rusqlite::Transaction` from the caller.
That design was rejected for two reasons.

The first is API surface. rusqlite types do not appear in the public API here.
Geometry is `impl geo_traits::GeometryTrait<T = f64>` and non-geometry values
are this library's own value types, so a major-version bump in rusqlite does
not cascade into every caller. Accepting a transaction object would put that
type in the signature of the most-used write path in the library.

The second is bookkeeping. The writer maintains a running bounding-box fold
and a `last_change` timestamp, and flushes both at one point. Owning the
transaction makes that point well defined.

You can still drive your own transaction through the raw connection.
Inheritance is what makes that work.

## Beginning without locking

`Layer::writer` opens a transaction without writing anything, and does not
take a lock. SQLite's `BEGIN DEFERRED` acquires nothing until the first
statement needs it. So the first failure a caller sees comes from the first
row, not from the call that returned the writer.

This matters most in two cases. On a read-only connection, opening a writer
succeeds and writing the first row fails. Under contention, the wait for
another connection's lock happens at the first write, not when the writer
opens. So a successful call to open a writer does not show that the file is
writable.

## Reading and writing on one connection at once

A writer and a feature cursor share the connection, so a scan can drive its
own updates: read a row, recompute a column, write it back.

SQLite does not define what such a scan sees. Its isolation documentation
says explicitly that a `SELECT` on one connection has no isolation from writes
on that same connection, and that an application "can UPDATE the current row or
any prior row, though doing so might cause that row to reappear in a
subsequent `sqlite3_step()`". SQLite promises only that the file will not be
harmed. It does not promise that the result set is stable, complete, or free
of repeats.

In practice the scan is stable when all three of these are true:

- the cursor is a plain table scan, not one driven by an index;
- the columns written are not ones the scan's index reads;
- the primary key is not written, since moving a row's id moves it within a
  rowid scan.

Avoid writing a geometry during a bounding-box cursor. That cursor is driven
by a join against the RTree, and writing a geometry moves the row inside that
index through the triggers. This is the pattern that makes a scan return rows
it has already returned. To recompute geometries, use two passes: collect the
feature ids, finish the scan, then write.

None of this is specific to this library, and none of it is a defect that
could be fixed here. It is SQLite's stated contract for one connection reading
and writing at once. The alternative, a second connection, has its own locking
to manage.

## Getting the work done

- [How to copy features between files without decoding geometry](../how-to/copy-features.md)
  shows a long write committed in batches, and what changes when the caller
  owns the transaction.
- [`GeoPackage::connection`](https://docs.rs/geopackage/latest/geopackage/struct.GeoPackage.html#method.connection)
  is the escape hatch this chapter is about.
