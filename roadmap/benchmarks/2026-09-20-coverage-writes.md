# What Requirement 10 costs per tile (M6 phase 2b)

Date: 2026-09-20. Machine: a Linux CI container, Intel Xeon @ 2.10 GHz, 4
cores, 16 GB, system SQLite 3.45.1, release build. The M4 tile figures were
taken on a different machine (an Apple M-series laptop), so **the absolute
rates here are not comparable with
[2026-07-27-tiles.md](2026-07-27-tiles.md)**. This measurement is about the
ratio, which is taken against a control on the same machine in the same run.

Reproduce with `cargo bench -p geopackage --bench tiles -- coverage/`.

## What was measured

A tiled gridded coverage needs a `gpkg_2d_gridded_tile_ancillary` row for
every tile (Requirement 10). The normative table definition gives no
`ON DELETE CASCADE`, so the writer maintains the pairing itself. It runs three
statements per tile (insert the tile, read back its id, insert the ancillary
row) where a pyramid's writer runs one.

Both arms write the same 341 addresses (a web mercator ladder, zoom 0 to 4)
with the same 4 KiB payload through a per-tile `put` inside one writer
transaction. The only difference is the writer.

| Writer | Median | Rate | Per tile |
|---|---|---|---|
| `TileWriter::put` (one statement) | 4.77 ms | ~71,600 tiles/sec | 14.0 µs |
| `CoverageWriter::put` (three statements) | 6.01 ms | ~56,700 tiles/sec | 17.6 µs |

**Requirement 10 costs about 26%** of per-tile write throughput: 3.6 µs a tile
on this machine.

## Where the cost goes

Three statements cost only 1.26 times as much as one. Most of a tile write is
not the statement but the page the payload lands on and the transaction around
it, and the two extra statements touch small rows in tables that stay hot. The
id read is the cheaper addition: a primary-key lookup on a row that SQLite has
just written. The ancillary insert is a nine-column row of numbers.

The code notes an alternative at the statement itself.
`INSERT ... RETURNING id` would fold the id read into the insert and save
perhaps half of the 3.6 µs, but would require SQLite 3.35 at runtime. This
crate links the system SQLite by default (D1) and imposes no runtime SQLite
version anywhere else, so the trade is a 13% write speedup against a new
minimum for every consumer. It was not taken. Revisit it if a minimum version
is ever declared for other reasons.

## What this does not measure

Batch writes. A coverage has no `write_all` yet, so there is no batch arm to
compare against the pyramid's. If one is added, extend this file. Payload size
is fixed at 4 KiB, which keeps the measurement about the container. A typical
float32 coverage tile is larger, which would dilute the ratio rather than
concentrate it.
