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
| `constraints/unenforced` | 159 ms | 1,257,000 rows/sec |
| `constraints/enforced` | 209 ms | 956,000 rows/sec |

**About 31% slower, roughly 250 ns per row for two checks.**

## Which implementation of `GLOB`

The glob form was first implemented here, as a matcher following
`patternCompare` in SQLite's `func.c`, with a property test holding it to
SQLite's answers. It was then replaced by asking SQLite itself, through a
`SELECT ?1 GLOB ?2` prepared once per writer.

The reason to prefer the engine is correctness rather than speed: the pattern
language has no definition beyond what SQLite does with it, we bundle SQLite,
and a copy of its rules can drift from the engine holding the file without
anything failing. Handing numbers to SQLite also gives them its own text
coercion, where the matcher had to approximate it with Rust's formatting.

Speed did not argue the other way. Per call, over 300,000 evaluations of
`[A-Z][A-Z]-*` against a matching ten-character value:

| Implementation | Per call |
|---|---|
| Hand-rolled matcher | 176.0 ns |
| `SELECT ?1 GLOB ?2`, prepared once | 137.7 ns |

**The engine is 22% faster**, which is not the expected direction until you
notice what each does: SQLite walks the UTF-8 bytes and allocates nothing,
while the matcher collected both pattern and value into `Vec<char>` before
starting. A statement step costs less than two allocations.

Measured with a throwaway harness rather than a committed benchmark, since the
question it answers was settled once.

## Why the default is off

The spec makes these constraints advisory: "These restrictions MAY be enforced
by SQL triggers or by code in applications that update GeoPackage data values"
(Annex F.9). A conforming file may therefore hold values its own constraints
forbid, and a writer that refused them by default would be imposing a rule the
format does not. 31% is also enough to be worth a caller's decision rather
than ours.

## A note on the first figures

An earlier run of the same benchmark reported 569 ms and 651 ms for the two
arms, a 14% difference. Those numbers were taken while the machine was running
the test suite, and the unenforced arm alone moved by a factor of 3.6 between
that run and this one. Only figures from a single quiet run are comparable;
the pair above is one such run, and the earlier pair is recorded here only so
the discrepancy is not mistaken for a regression.
