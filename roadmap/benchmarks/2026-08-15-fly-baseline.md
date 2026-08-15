# Re-baseline on a dedicated cloud machine

The Performance tables in the README were measured on an Apple M2 Pro
workstation. That host stopped being able to satisfy the protocol's idle-host
requirement: on 2026-08-15 two full `scripts/bench_datasets.sh` runs under
load averages of 5 to 10 disagreed with the published table on arms the code
had not changed (admin write 7.1 to 7.9 s against the published 4.8 s). The
figures are therefore re-measured on a machine type anyone can rent, which
also makes them reproducible on demand.

## The admin write question, settled first

Before re-baselining, the local discrepancy had to be attributed. An
interleaved A/B on the benchmark machine, alternating five runs each of the
admin write arm built from commit 020e2b7 (the commit whose figures the
README carried) and from v0.9.0:

| ref | median | min | max |
|---|---|---|---|
| 020e2b7 | 23,490 ms | 22,923 | 23,797 |
| v0.9.0 | 23,429 ms | 23,366 | 23,754 |

No code change: the local readings were host load. Interleaving is the
method to reuse for any future "did this regress" question, because it
cancels drift that repeated runs of a single binary cannot.

## Environment

- Fly.io Machine, `performance-8x`: 8 dedicated AMD EPYC vCPUs, 16 GB.
- Region `ams`, Linux 6.12.91-fly, Ubuntu 24.04 userland.
- Release build, bundled SQLite, warm page cache, medians over 3 repetitions
  (5 for the tiles arms), host otherwise idle.
- Provisioned and driven by `scripts/bench_fly.sh`; datasets uploaded from
  the previously converted local copies, byte-identical to the 2026-07-25
  originals, because the dataset hosts are not reachable from Fly's network.
- The peak-RSS arms did not run: the probe in `bench_datasets.sh` is
  macOS-specific.

Single-thread CPU work runs roughly 1.5x to 3x slower on these cores than on
the M2 Pro, so nothing in this table is comparable to the previous one; that
is the cost of a reproducible baseline, paid once.

## Datasets, medians of 3 (measured at commit 0497b4f)

| operation | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| scalar read, `cursor` | 5.8 s | 9.5 s | 3.3 s |
| columnar read, `read_arrow` | 2.7 s | 3.1 s | 3.6 s |
| the same read, single-threaded | 3.8 s | 7.0 s | 4.6 s |
| write from Arrow batches | 30.2 s | 30.0 s | 23.1 s |
| the same write, index built as it goes | 39.6 s | 35.9 s | 23.6 s |
| `create_spatial_index` afterwards instead | 17.6 s | 12.9 s | 2.0 s |
| bounding-box query, indexed | 134 ms | 289 ms | 230 ms |
| the same query with no index | 2.6 s | 3.5 s | 1.9 s |

Two structural differences from the M2 table, beyond the uniform slowdown:

- The admin index build is absolutely faster here (2.0 s against 6.3 s on
  the M2). That arm is dominated by the branch-free envelope scan over
  2.4 GB of multipolygon coordinates; a plausible cause is better
  autovectorisation on this target, but that is a hypothesis, not a
  measurement.
- Building the index during the write is 6% to 17% cheaper than the two
  steps run separately, against 17% to 35% on the M2: the insert side grew
  more expensive relative to the envelope side.

## Tiles, zoom 0 to 7, 21,845 tiles of 4 KiB (medians of 5)

| operation | time | rate |
|---|---|---|
| streaming scan | 243 ms | ~90,000 tiles/s |
| random read by address | 359 ms | ~61,000 tiles/s |
| write through `write_all` | 315 ms | ~69,000 tiles/s |
| GDAL `gdalinfo -checksum` (decodes to pixels) | 21.6 s | ~1,000 tiles/s |

The GDAL arm decodes every PNG and this crate returns stored bytes; they are
different operations, quoted side by side because a tile server needs both
numbers. Note the ordering flip against the M2, where random-by-address beat
the streaming scan: here the scan wins.
