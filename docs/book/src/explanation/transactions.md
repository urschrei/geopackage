# Transactions, and who commits

SQLite has no nested transactions. A `BEGIN` issued while one is already open
fails with "cannot start a transaction within a transaction". That single fact
shapes how every write path in this library begins its work, and it is worth
understanding before you open a transaction of your own on the underlying
connection.

## The problem it created

Early on, every write path here issued a `BEGIN` unconditionally. That is fine
as long as the library is the only thing driving the connection, and it stops
being fine the moment a caller reaches for the escape hatch.

`GeoPackage::connection` returns the underlying rusqlite connection, because
SQLite is the query engine and anything the API does not cover should be a
query away. A caller who used that hatch to begin a transaction and then went
back to the ordinary API received the nesting error rather than their writes.
The C ABI had the same problem in a sharper form: it could not offer
`gpkg_begin` and `gpkg_commit` at all, because the sequence a C consumer would
reasonably write could not work.

## Inheriting rather than nesting

The resolution is that a write path checks whether a transaction is already
open and, if one is, runs its statements inside it.

The atomicity is unchanged. The statements are still grouped, they still
commit together, and a failure still rolls them back together. What changes is
who decides when that happens, and that passes to whoever issued the `BEGIN`.

Every write path in the library behaves this way, from a single
`FeatureWriter` insert to a bulk `write_all` and the extent recording
described in [the extent chapter](extent.md).

## Why not savepoints

Savepoints are the other way to get nesting out of SQLite, and they were
considered and rejected.

`rusqlite::Connection::savepoint` needs `&mut Connection`, and this library has
only `&Connection`, because the read path shares it. Savepoints would
therefore have to be driven as raw `SAVEPOINT` and `RELEASE` text. That is
more machinery, and it buys a finer rollback granularity that nobody has asked
for. It is not a closed question: if a need arises, the transaction type is
the one place that would have to change.

## What inheritance costs the caller

Three consequences follow, and they apply to every write path rather than only
to the feature writer.

**Commit does not commit** when the transaction was inherited. It still does
everything else a commit does, including flushing the `gpkg_contents`
`last_change` and bounding box, but the durable commit is issued by the caller.

**Dropping a writer does not roll back** when the transaction was inherited, so
an error part-way through leaves partial work staged in the caller's
transaction for them to discard.

**`write_all`'s `batch_size` stops bounding transactions**, because every batch
belongs to the caller's transaction. It bounds nothing else either: the rows
are written in the same order, through the same statements.

None of this is detectable from a writer, and deliberately so. A caller who
opened a transaction knows they did; a caller who did not cannot reach this
behaviour at all.

## Why the writer owns a transaction rather than taking one

An alternative shape would be for the writer to accept a
`rusqlite::Transaction` from the caller. It was not chosen, for two reasons.

The first is API surface. rusqlite types do not appear in the public API here:
geometry is `impl geo_traits::GeometryTrait<T = f64>` and non-geometry values
are this library's own value types, so a major-version bump in rusqlite does
not cascade into every caller. Accepting a transaction object would put that
type in the signature of the most-used write path in the library.

The second is bookkeeping. The writer maintains a running bounding-box fold
and a `last_change` timestamp, and both are flushed at one point. Owning the
transaction is what makes that point well defined.

A caller who wants to drive their own transaction still can, through the raw
connection, and inheritance is what makes that work rather than fail.

## Beginning without locking

`Layer::writer` opens a transaction without writing anything, and does not
take a lock. SQLite's `BEGIN DEFERRED` acquires nothing until the first
statement needs it, so the first failure a caller sees lands on the first row
rather than on the call that returned the writer.

This matters most on a read-only connection, where opening a writer succeeds
and the first row written fails, and under contention, where the wait for
another connection's lock happens where the work does. It is worth knowing
because the natural reading of "opening a writer succeeded" is that the file
is writable, and it is not what that call has established.

## Reading and writing on one connection at once

A writer and a feature cursor share the connection, so a scan can drive its
own updates: read a row, recompute a column, write it back.

SQLite does not define what such a scan sees. Its isolation documentation is
explicit that a `SELECT` on one connection has no isolation from writes on
that same connection, and that an application "can UPDATE the current row or
any prior row, though doing so might cause that row to reappear in a
subsequent `sqlite3_step()`". The safety it does promise is only that the file
will not be harmed. The result set is not promised to be stable, complete, or
free of repeats.

In practice the scan is stable when all three of these are true: the cursor is a
plain table scan rather than one driven by an index; the columns written are
not ones the scan's index reads; and the primary key is not written, since
moving a row's id moves it within a rowid scan.

The case to avoid is writing a geometry during a bounding-box cursor. That
cursor is driven by a join against the RTree, and writing a geometry moves the
row inside that index through the triggers, which is exactly the shape that
makes a scan return rows it has already returned. Recomputing geometries is
better done in two passes: collect the feature ids, finish the scan, then
write.

None of this is specific to this library, and none of it is a defect that
could be fixed here. It is SQLite's stated contract for one connection reading
and writing at once, and the alternative, a second connection, brings its own
locking to think about instead.

## Getting the work done

- [How to copy features between files without decoding geometry](../how-to/copy-features.md)
  shows a long write committed in batches, and what changes when the caller
  owns the transaction.
- [`GeoPackage::connection`](https://docs.rs/geopackage/latest/geopackage/struct.GeoPackage.html#method.connection)
  is the escape hatch this chapter is about.
