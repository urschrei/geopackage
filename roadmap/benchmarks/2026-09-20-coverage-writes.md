# What Requirement 10 costs per tile (M6 phase 2b)

Date: 2026-09-20. Machine: **a Linux CI container**, Intel Xeon @ 2.10 GHz, 4
cores, 16 GB, system SQLite 3.45.1, release build. Not the machine the M4 tile
figures were taken on (an Apple M-series laptop), so **the absolute rates here
are not comparable with
[2026-07-27-tiles.md](2026-07-27-tiles.md)**. The ratio is what this
measurement is for, and the ratio is measured against a control taken on the
same machine in the same run.

Reproduce with `cargo bench -p geopackage --bench tiles -- coverage/`.

## What was measured

A tiled gridded coverage needs a `gpkg_2d_gridded_tile_ancillary` row for
every tile (Requirement 10), and the normative table definition gives no
`ON DELETE CASCADE`, so the writer maintains the pairing itself: three
statements per tile (insert the tile, read back its id, insert the ancillary
row) where a pyramid's writer does one.

Both arms write the same 341 addresses (a web mercator ladder, zoom 0 to 4)
with the same 4 KiB payload through a per-tile `put` inside one writer
transaction. The only difference is which writer.

| Writer | Median | Rate | Per tile |
|---|---|---|---|
| `TileWriter::put` (one statement) | 4.77 ms | ~71,600 tiles/sec | 14.0 µs |
| `CoverageWriter::put` (three statements) | 6.01 ms | ~56,700 tiles/sec | 17.6 µs |

**Requirement 10 costs about 26%** of per-tile write throughput: 3.6 µs a tile
on this machine.

## Reading it

Three statements for the price of 1.26, because most of a tile write is not
the statement: it is the page the payload lands on and the transaction around
it, and the two extra statements touch small rows in tables that stay hot. The
id read is the cheaper of the two additions — a primary-key lookup on a row
SQLite has just written — and the ancillary insert is a nine-column row of
numbers.

That is the figure to weigh against the alternative the code notes at the
statement itself: `INSERT ... RETURNING id` would fold the id read into the
insert and save perhaps half of the 3.6 µs, at the price of requiring SQLite
3.35 at runtime. This crate links the system SQLite by default (D1) and
imposes no runtime SQLite version anywhere else, so the trade is a 13% write
speedup against a new minimum for every consumer. Not taken. Revisit if a
minimum version is ever declared for other reasons.

## What this does not measure

Batch writes: a coverage has no `write_all` yet, so there is no batch arm to
compare against the pyramid's. If one is added, this is the file to extend.
Payload size is fixed at 4 KiB, which keeps the measurement about the
container; a real float32 coverage tile is larger, which would dilute the
ratio rather than concentrate it.
