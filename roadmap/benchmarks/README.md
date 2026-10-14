# Benchmarks

geopackage 0.11.0 against GDAL 3.13.3, on three published datasets.

## Machine and software

- Fly.io Machine, `performance-8x`: 8 dedicated AMD EPYC vCPUs, 16 GB, Linux
  6.12, Ubuntu 24.04.
- geopackage 0.11.0, release build, bundled SQLite 3.53.2.
- GDAL 3.13.3 from conda-forge, linking SQLite 3.54.0.
- The page cache is warm for every run.

## Datasets

All three are unindexed GeoPackages written by `ogr2ogr`.

| | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| source | Microsoft Building Footprints, California | HydroRIVERS v1.0, global | GADM 4.1, global |
| rows | 11,542,912 | 8,477,883 | 356,508 |
| geometry | Polygon | LineString | MultiPolygon |
| attribute columns | 4 | 16 | 54 |
| file | 2.37 GB | 2.03 GB | 2.74 GB |
| bytes per row | 205 | 240 | 7,687 |

## Library

Median of 3 runs, in seconds unless marked. The two sides ran in alternating
order.

| operation | `buildings`, geopackage | GDAL | `rivers`, geopackage | GDAL | `admin`, geopackage | GDAL |
|---|---|---|---|---|---|---|
| scalar read | 6.9 | 8.3 | 10.8 | 9.1 | 4.1 | 3.1 |
| Arrow read | 3.0 | 0.9 | 3.4 | 1.3 | 3.8 | 4.5 |
| Arrow read, one thread | 3.9 | 3.0 | 7.2 | 4.6 | 4.6 | 6.8 |
| box query, no index | 2.7 | 1.7 | 3.7 | 1.4 | 2.0 | 1.1 |
| index build | 15.6 | 28.3 | 11.5 | 20.3 | 2.1 | 1.5 |
| box query, indexed | 157 ms | 164 ms | 350 ms | 318 ms | 255 ms | 226 ms |
| write | 20.1 | 26.3 | 19.7 | 24.6 | 7.2 | 12.0 |
| write, index built during the write | 30.3 | 42.7 | 27.5 | 33.2 | 7.4 | 12.4 |

Peak resident memory, median of 3 runs, in MiB:

| operation | `buildings`, geopackage | GDAL | `rivers`, geopackage | GDAL | `admin`, geopackage | GDAL |
|---|---|---|---|---|---|---|
| Arrow read | 86 | 143 | 112 | 162 | 1,762 | 2,682 |
| index build | 981 | 459 | 723 | 425 | 37 | 72 |
| write | 1,886 | 1,939 | 1,850 | 1,673 | 2,641 | 2,719 |

What each side does:

- **Scalar read**: read every feature and build its geometry.
  [`Layer::cursor`] and `Geometry::to_geo`; `OGR_L_GetNextFeature`.
- **Arrow read**: consume every batch of the layer. [`Layer::read_arrow`];
  `OGR_L_GetArrowStream`. Both use up to 4 threads by default.
- **Box query**: read every feature in the box and build its geometry. The
  boxes are San Francisco (70,130 features), part of the Amazon basin
  (180,544) and western Europe (36,556). [`Layer::cursor_in`] returns the
  features whose envelope intersects the box; GDAL returns 2 fewer on
  `admin`. The indexed query uses the index that the same side built.
- **Index build**: [`Layer::create_spatial_index`];
  `SELECT CreateSpatialIndex(...)`. The index adds 444, 325 and 13 MiB to the
  three files, and GDAL's adds 639, 562 and 20 MiB.
- **Write**: write the layer's Arrow batches, already in memory, to a new
  file in one transaction. [`Layer::write_arrow`]; `OGR_L_WriteArrowBatch`.
  Both sides write with `synchronous = OFF`.

[`Layer::cursor`]: https://docs.rs/geopackage/0.11.0/geopackage/struct.Layer.html#method.cursor
[`Layer::read_arrow`]: https://docs.rs/geopackage/0.11.0/geopackage/struct.Layer.html#method.read_arrow
[`Layer::cursor_in`]: https://docs.rs/geopackage/0.11.0/geopackage/struct.Layer.html#method.cursor_in
[`Layer::create_spatial_index`]: https://docs.rs/geopackage/0.11.0/geopackage/struct.Layer.html#method.create_spatial_index
[`Layer::write_arrow`]: https://docs.rs/geopackage/0.11.0/geopackage/struct.Layer.html#method.write_arrow

## `gpkg copy`

Copy every layer of a dataset to a new file. Mean ± standard deviation of 5
runs, through hyperfine; peak resident memory over 2 runs.

| | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| `gpkg copy` | 26.0 ± 1.2 s | 25.3 ± 0.9 s | 11.1 ± 0.8 s |
| `ogr2ogr` | 25.1 ± 0.2 s | 23.9 ± 0.3 s | 10.6 ± 0.2 s |
| `gpkg copy`, peak memory | 15 MiB | 21 MiB | 226 to 227 MiB |
| `ogr2ogr`, peak memory | 94 MiB | 95 MiB | 1.29 GiB |

The `ogr2ogr` command is
`ogr2ogr -f GPKG -lco SPATIAL_INDEX=NO -preserve_fid -gt 10000`, which does
what `gpkg copy` does with an unindexed source: keep the fids, commit every
10,000 rows, and build no index. `ogr2ogr` reports that it copies through
`WriteArrowBatch` and that the GeoPackage driver uses 4 threads. `gpkg copy`
reads on one thread and writes on another, and copies every value unchanged.
Neither syncs the output to disk.

## Reproducing

[`scripts/bench_fly.sh`](../../scripts/bench_fly.sh) creates the machine and
runs both benchmarks on it:

```sh
scripts/bench_fly.sh setup     # once: the app and the data volume
scripts/bench_fly.sh start
scripts/bench_fly.sh upload    # or `fetch`, to download the datasets
scripts/bench_fly.sh bench     # the library, at origin/main
scripts/bench_fly.sh copy      # `gpkg copy`, at origin/main
scripts/bench_fly.sh stop
```

To run them on another machine with GDAL installed:

```sh
scripts/bench_datasets.sh fetch ~/benchdata
scripts/bench_datasets.sh run ~/benchdata 3
cargo build --release -p geopackage-cli
scripts/compare_gdal_copy.sh ~/benchdata target/release/gpkg 5
```

[`scripts/bench_datasets.sh`](../../scripts/bench_datasets.sh) times the
library with `geopackage/examples/dataset_bench.rs` and
[`scripts/gdal_dataset_bench.c`](../../scripts/gdal_dataset_bench.c), and
uses the GDAL that `gdal-config` names; set `GDAL_CONFIG` to use another.
[`scripts/compare_gdal_copy.sh`](../../scripts/compare_gdal_copy.sh) times
`gpkg copy` and `ogr2ogr` with hyperfine.
