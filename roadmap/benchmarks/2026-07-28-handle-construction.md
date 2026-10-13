# Handle construction against per-call work (M5 phase 9)

Date: 2026-07-28. Machine: Apple M2 Pro, 12 cores, 16 GB, macOS 24.6.0, release
build, bundled SQLite, warm page cache. Reproduce with
`cargo bench -p geopackage --bench handles`.

## Why this was measured

`Layer<'a>` and `TilePyramid<'a>` borrow the `GeoPackage` they came from. A C
ABI handle cannot hold such a borrow: a C caller keeping a layer handle past its
container handle is the use-after-free the lifetime prevents. Phase 9 therefore
has to choose between two designs:

- **Owned.** `Layer` and `TilePyramid` own an `Arc<GeoPackage>`, and an FFI
  handle owns one directly.
- **Re-derive.** They stay borrowed, and an FFI handle stores the parent plus a
  table name, rebuilding the handle inside every call.

The choice is a performance question, so it was measured.

## What was measured

| Group | Operation | Median |
|---|---|---|
| `construct` | `gpkg.layer`, 4 columns | 36.99 µs |
| `construct` | `gpkg.layer`, 54 columns | 51.21 µs |
| `construct` | `gpkg.tiles` | 40.63 µs |
| `construct` | `Arc::clone` and drop | 3.77 ns |
| `cheap_calls` | `layer.extent` | 5.48 µs |
| `cheap_calls` | `layer.has_spatial_index` | 13.79 µs |
| `features_in` | 1 row returned | 24.65 µs |
| `features_in` | 15 rows returned | 28.33 µs |
| `features_in` | 126 rows returned | 54.97 µs |
| `features_in` | 1071 rows returned | 289.24 µs |
| `tiles` | `get_tile` | 5.22 µs |
| `tiles` | `get_tile_into` | 5.06 µs |

The tile figures agree with
[2026-07-27-tiles.md](2026-07-27-tiles.md). 5.22 µs is about 192,000 tiles/sec,
against the 146,000 recorded there for a zoom 7 pyramid. The difference is that
this pyramid is smaller and fully resident. The relative conclusions below hold
with either figure.

## Findings

Construction is a near-constant 37 to 51 µs, because it is a fixed set of
queries: a `gpkg_contents` lookup, a table-name resolution, a
`PRAGMA table_info`, a `gpkg_data_columns` query, and a `Vec<Column>` clone. It
scales with column count, not with row count.

Because construction cost is constant, the relative overhead of re-deriving is
inversely proportional to the cost of the call:

| Call | Cost | Rebuild adds |
|---|---|---|
| `layer.schema()` field access | free, no IO | unbounded |
| `pyramid.get_tile` | 5.22 µs | +778% |
| `layer.extent` | 5.48 µs | +675% |
| `layer.has_spatial_index` | 13.79 µs | +268% |
| `features_in`, 1 row | 24.65 µs | +150% |
| `features_in`, 15 rows | 28.33 µs | +131% |
| `features_in`, 126 rows | 54.97 µs | +67% |
| `features_in`, 1071 rows | 289.24 µs | +13% |

The cheapest and most frequently called operations pay the most, which is the
wrong way round. Streaming paths (the Arrow reader, the feature cursor and the
tile cursor) are unaffected, because their handle is built once and amortised
over the scan. The cost falls entirely on the random-access API, the part a C
consumer uses most.

`get_tile` is the worst case and also the most frequently called entry point in
the crate, since a tile server calls it once per request. Re-deriving takes it
from 5.22 µs to 45.9 µs, so 192,000 tiles/sec becomes about 21,800. The C ABI's
headline tile figure would be roughly **8.8x worse than the Rust one**, purely
because of handle design.

Small `features_in` results are the second worst case, and they are the common
one. An interactive map or a feature-info request returns single-digit to tens
of rows, so the representative overhead for that path is 131% to 150%. An
earlier estimate of 30% came from a query returning every row of a 1000-row
table, which is the most favourable case, not a representative one.

## The other side of the trade

An owned handle adds one `Arc::clone` per construction: **3.77 ns against a
36,985 ns construction**, about one part in ten thousand. Field access is
unchanged, since `Arc<T>` and `&T` are both a single dereference.

Nothing constructs a handle in a hot loop. The only construction sites in the
library are `create_layer`, `create_tile_pyramid`, `layers()`, and one per
worker thread in the parallel Arrow read (`geopackage/src/arrow.rs`, which opens
its own connection per thread). No path builds one per row or per batch.

On performance, the argument runs only one way. Owning costs Rust callers
nothing measurable and saves FFI callers between 13% and 778% depending on the
call.

## What this does not settle

Two things this file does not measure decide against owning. Both are recorded
in [07-m5-extensions-and-1.0.md](../07-m5-extensions-and-1.0.md) under phase 9.

`Arc<T>` is `Send` only when `T` is `Sync`, and `Connection` is not `Sync`. A
`GeoPackage` built as `Arc<Inner>` therefore loses the `Send` it has today,
which D1's `spawn_blocking` async plan requires. No benchmark can address this,
and it is the strongest argument against the owned options.

The other is the close contract. `GeoPackage::close` takes `self`. For a handle
that opted into WAL, it checkpoints to `TRUNCATE`, resets the journal mode to
`DELETE` and drops the connection, so a handed-over file is a single file with
no sidecars. Once a `Layer` or `TilePyramid` can hold a strong reference,
consuming the `GeoPackage` no longer implies dropping the connection.

This file establishes only that performance does not count against owning.
That does not mean owning wins, and on the two arguments above it does not.
