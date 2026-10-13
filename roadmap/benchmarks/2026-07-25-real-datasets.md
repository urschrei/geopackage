# Three published datasets, measured end to end

These are the figures published in the README. Earlier benchmarks measured
generated fixtures. Those answer "did this change make the code faster", but
say nothing about what a caller should expect from a file they already have.
Real data differs from the point and polygon fixtures on the two axes these
paths are most sensitive to: vertex count per geometry, and how unevenly the
features are spread.

Apple M2 Pro, 12 cores, 16 GB, macOS 15.6.1, release build, bundled SQLite
3.53.2. `scripts/bench_datasets.sh run <dir> 3`, which reports the median of
three repetitions per arm.

## The datasets

The datasets were chosen to sit at different points on both axes, not for
size. All three were converted once with GDAL 3.12.3 to an unindexed
GeoPackage, so the index-build arm starts from the same state everywhere and
the read arms are not reading index pages.

| | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| source | Microsoft Building Footprints, California | HydroRIVERS v1.0, global | GADM 4.1, global |
| rows | 11,542,912 | 8,477,883 | 356,508 |
| geometry | Polygon | LineString | MultiPolygon |
| attribute columns | 4 | 16 | 54 |
| file | 2.37 GB | 2.03 GB | 2.74 GB |
| bytes per row | 205 | 240 | 7,687 |

`buildings` has many rows with almost no geometry each. `admin` has few rows
that are nearly all coordinates. `rivers` sits between them and is the closest
of the three to ordinary vector data.

## Results

| operation | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| columnar read, `read_arrow` | 2.1 s | 1.9 s | ~2.9 s |
| scalar read, `cursor` | 4.3 s | 6.9 s | 2.4 s |
| write from Arrow batches | 12.7 s | 12.9 s | 8.4 s |
| the same write, index built as it goes | 31.0 s | 21.7 s | 8.9 s |
| `create_spatial_index` on the written table | 26.0 s | 14.6 s | 8.5 s |
| bounding-box query, indexed | 80 ms | 199 ms | 178 ms |
| the same query, no index | 1.7 s | 2.2 s | 1.3 s |
| features the query returned | 70,130 | 180,544 | 36,556 |

## Findings

**The columnar read is bandwidth-bound at roughly 1 GB/s.** Dividing file size
by read time gives 1.13, 1.07 and 0.95 GB/s across three layers whose rows
differ by a factor of 37 in size. Per row, the same figures are 5.5M, 4.5M and
123k rows/s, which hides the pattern. Time scales with bytes, not rows.

**Columnar beats scalar by 2x to 4x when there are enough rows to amortise
it.** It is 2.1x faster on `buildings` and 3.6x on `rivers`. On `admin` the
two are level (2.4 s scalar against 1.7 to 5.1 s columnar): they cannot be
separated on this host, and neither can be said to win. `admin` differs in
having 356k rows, a twenty-fourth of `rivers`, so per-read setup is spread over
far fewer of them.

The gap does not grow with column count: `admin` has 54 columns to `rivers`'
16 and shows the smallest gap of the three.

**The index costs about 40 bytes per row, whatever the geometry.** File growth
gives 40.4, 40.3 and 38.7 bytes per row across the three. The cost tracks row
count and nothing else, so `admin` pays 13.8 MB on a 2.74 GB file while
`buildings` pays 466 MB on a 2.37 GB one.

**Index build time also tracks rows, not bytes.** The rates are 444k, 581k and
42k rows/s. `admin`'s rate is an order of magnitude lower because the `ST_*`
envelope scan has to walk every coordinate of a 7 kB multipolygon to find its
box. That scan is the one part of the build paid per vertex. The gate
documented in
[2026-07-24-gdal-like-for-like.md](2026-07-24-gdal-like-for-like.md) is still
roughly half of each of these figures.

**Building the index during the write is cheaper than building it after.**
`buildings` writes in 12.7 s and indexes in 26.0 s, 38.7 s in total, against
31.0 s when `write_all` builds it in the same pass. The bulk path reuses the
envelopes it computed while encoding, so it skips the `ST_*` scan entirely.
That saves 20% on `buildings`, 21% on `rivers` and 47% on `admin`, which gains
most because the scan it avoids is the per-vertex one. This is why
`create_layer` leaves an empty index in place by default.

**The spatial index is worth 7x to 21x on these queries.** It gives 21x on
`buildings`, where the box selects 0.6% of 11.5M rows, and 7.3x on `admin`,
where it selects 10% of 356k. The gain is proportional to how much the index
lets the reader skip.

## The one unstable figure

`admin`'s columnar read is quoted as approximate because it varied. Across
today's runs it ranged from 1.7 s to 5.1 s, and run order does not explain it.
Most cells reproduced within 5% across two full runs. The exceptions were all
read arms, and all on the two largest files: `rivers`' scalar scan varied by
20% and `admin`'s three read arms by 12% to 32%. Each was then re-measured
over four or five rounds, which produced the quoted figures. Every write,
index and query cell reproduced within 5% the first time.

The cause is memory, not the read path. `read_arrow` defaults to 65,536 rows
per batch, capped by `max_batch_bytes`. At 7.7 kB per row `admin` reaches
neither limit in a useful way. It produces six batches of roughly 450 MB, and
the default four reader threads hold one each, so about 1.8 GB is in flight
before any of it reaches the caller. On a 16 GB host already 5 GB into swap,
whether that fits depends on what else is resident, and the timing follows.

Two things follow for callers with geometry-heavy layers. First,
`ArrowReadOptions` exposes `with_batch_size` and `with_max_batch_bytes` for
this case, and lowering either bounds the memory in flight. Second,
`with_threads(1)` measured faster than the default in three of five paired
comparisons on `admin`, while on `rivers` threading is worth a consistent
1.8x. Three of five is not a finding. It is quoted as evidence that the
ordering itself is unstable here, not that one thread is better. The
automatic choice of `min(4,
available parallelism)` assumes batches small enough that holding one per
thread is free, and `admin` breaks that assumption.

Neither is a defect in the reader, and one host's memory pressure did not
justify changing a default. This section is the evidence behind the README's
caveat.

## Reproducing

```sh
scripts/bench_datasets.sh fetch ~/benchdata   # ~5 GB of downloads
scripts/bench_datasets.sh run   ~/benchdata 3
```

The fetch step converts each source to an unindexed GeoPackage with `ogr2ogr`.
The run step needs only the three `.gpkg` files. GADM is already distributed
as a GeoPackage, but an indexed one written by a 2022 GDAL, so it is
re-converted along with the other two.
