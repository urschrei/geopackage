# What enforcing `gpkg_schema` constraints costs on the write path (M5 phase 3)

Date: 2026-07-27. Machine: Apple M-series laptop, macOS 24.6.0, release build,
bundled SQLite. Reproduce with
`GPKG_BENCH_ROWS=200000 cargo bench -p geopackage --bench write -- constraints`.

## What was measured

200,000 point rows written through `write_all` into an unindexed layer with two
constrained value columns: a TEXT column under the glob `[A-Z][A-Z]-*`, and an
INTEGER column under an inclusive range. Both arms get the identical file, with
the same constraints declared; only `OpenOptions::enforce_column_constraints`
differs, so what is measured is the checking rather than the having.

Every row satisfies both constraints, so this is the cost of a check that
passes. A check that fails ends the write, and its cost does not matter.

| Arm | Median | Rate |
|---|---|---|
| `constraints/unenforced` | 569 ms | 351,000 rows/sec |
| `constraints/enforced` | 651 ms | 307,000 rows/sec |

**14% slower, about 410 ns per row for two checks.**

## Where the cost is

Both checks are per value, and neither touches SQLite: the constraints are
resolved once when the writer is built, and the row path then walks its values
against them. The range check on an integer is a pair of comparisons and
allocates nothing. The glob check is the expensive half: `glob_match` collects
both the pattern and the value into `Vec<char>` before matching, so a checked
text value costs two allocations and a UTF-8 decode.

That is worth knowing but was not worth pre-empting. The pattern is a constant
per constraint and could be decoded once per writer rather than once per row,
which would remove half of it; the value's decode is harder to avoid, since
the matcher needs indexed access for backtracking. Neither is on the default
path: enforcement is opt-in, and a layer with no constraints pays one branch
per row.

## Why the default is off

The spec makes these constraints advisory: "These restrictions MAY be enforced
by SQL triggers or by code in applications that update GeoPackage data values"
(Annex F.9). A conforming file may therefore hold values its own constraints
forbid, and a writer that refused them by default would be imposing a rule the
format does not. 14% is also enough to be worth a caller's decision rather than
ours.
