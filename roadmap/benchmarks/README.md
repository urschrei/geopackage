# Benchmarks

geopackage at `main` 24707b4 (after 0.11.0) against GDAL 3.13.3, on subsets
of three published datasets.

## Machine and software

- Fly.io Machine, `performance-8x`: 8 dedicated AMD EPYC vCPUs, 16 GB, Linux
  6.12, Ubuntu 24.04.
- geopackage release build, bundled SQLite 3.53.2.
- GDAL 3.13.3 from conda-forge, linking SQLite 3.54.0.
- The page cache is warm for every run.

## Datasets

Each subset is the first rows of an unindexed GeoPackage that `ogr2ogr`
wrote from the published data.

| | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| source | Microsoft Building Footprints, California | HydroRIVERS v1.0, global | GADM 4.1, global |
| rows in the subset | 1,154,291 of 11,542,912 | 847,788 of 8,477,883 | 15,000 of 356,508 |
| geometry | Polygon | LineString | MultiPolygon |
| attribute columns | 4 | 16 | 54 |
| subset file | 236 MB | 212 MB | 251 MB |
| bytes per row | 205 | 250 | 16,756 |

## Results

Median of 3 runs, in milliseconds. The two sides ran in alternating order.

| operation | `buildings`, geopackage | GDAL | `rivers`, geopackage | GDAL | `admin`, geopackage | GDAL |
|---|---|---|---|---|---|---|
| scalar read | 603 | 785 | 859 | 917 | 253 | 245 |
| Arrow read | 158 | 152 | 269 | 191 | 351 | 607 |
| Arrow read, one thread | 331 | 331 | 623 | 530 | 340 | 605 |
| box query, no index | 216 | 173 | 202 | 178 | 95 | 98 |
| index build | 1,315 | 2,047 | 982 | 1,841 | 165 | 115 |
| box query, indexed | 9.6 | 14.6 | 48.4 | 56.7 | 2.4 | 5.3 |
| write | 1,914 | 2,355 | 1,850 | 2,188 | 515 | 782 |
| write, index built during the write | 2,733 | 3,504 | 2,457 | 3,006 | 519 | 698 |
| copy | 1,999 | 2,551 | 2,106 | 2,351 | 662 | 1,200 |

Peak resident memory, median of the same runs, in MiB:

| operation | `buildings`, geopackage | GDAL | `rivers`, geopackage | GDAL | `admin`, geopackage | GDAL |
|---|---|---|---|---|---|---|
| Arrow read | 77 | 140 | 92 | 134 | 240 | 406 |
| index build | 105 | 89 | 79 | 86 | 24 | 62 |
| write | 205 | 267 | 211 | 234 | 290 | 406 |
| copy | 15 | 92 | 19 | 93 | 129 | 243 |

What each side does:

- **Scalar read**: read every feature and build its geometry.
  [`Layer::cursor`] and `Geometry::to_geo`; `OGR_L_GetNextFeature`.
- **Arrow read**: consume every batch of the layer. [`Layer::read_arrow`];
  `OGR_L_GetArrowStream`. Both use up to 4 threads by default.
- **Box query**: read every feature in the box and build its geometry. The
  boxes are San Francisco (6,483 features), part of East Africa (27,907) and
  western Europe (101). [`Layer::cursor_in`] returns the features whose
  envelope intersects the box. The indexed query uses the index that the same
  side built.
- **Index build**: [`Layer::create_spatial_index`];
  `SELECT CreateSpatialIndex(...)`. The index adds 43, 31 and 0.6 MiB to the
  three files, and GDAL's adds 62, 55 and 0.8 MiB.
- **Write**: write the layer's Arrow batches, already in memory, to a new
  file in one transaction. [`Layer::write_arrow`]; `OGR_L_WriteArrowBatch`.
  Both sides write with `synchronous = OFF`.
- **Copy**: `gpkg copy` and
  `ogr2ogr -f GPKG -lco SPATIAL_INDEX=NO -preserve_fid -gt 10000`, which keep
  the fids, commit every 10,000 rows and build no index. Timed from outside
  the process. Neither syncs the output to disk.

[`Layer::cursor`]: https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.cursor
[`Layer::read_arrow`]: https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow
[`Layer::cursor_in`]: https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.cursor_in
[`Layer::create_spatial_index`]: https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.create_spatial_index
[`Layer::write_arrow`]: https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.write_arrow

## Reproducing

[`scripts/bench_fly.sh`](../../scripts/bench_fly.sh) creates the machine,
makes the subsets, and runs the benchmark at a local jj revision:

```sh
scripts/bench_fly.sh setup     # once: the app and the data volume
scripts/bench_fly.sh start
scripts/bench_fly.sh upload    # or `fetch`, to download the datasets
scripts/bench_fly.sh bench @   # the working copy
scripts/bench_fly.sh stop
```

GDAL's results are kept on the volume, one file per GDAL version, so a run
with them cached measures this crate's side only and takes about 90 s.

To run it on another machine with GDAL installed:

```sh
scripts/bench_datasets.sh fetch ~/benchdata
scripts/bench_datasets.sh subset ~/benchdata ~/benchdata/subsets
scripts/bench_datasets.sh run ~/benchdata/subsets 3
```

[`scripts/bench_datasets.sh`](../../scripts/bench_datasets.sh) times the
library with `geopackage/examples/dataset_bench.rs` and
[`scripts/gdal_dataset_bench.c`](../../scripts/gdal_dataset_bench.c), and the
copy with `gpkg copy` and `ogr2ogr`. It uses the GDAL that `gdal-config`
names; set `GDAL_CONFIG` to use another.
