# Whether a threaded filtered columnar read would pay

Date: 2026-08-02. Machine: Apple Silicon macOS (Darwin 24.6, arm64), the same
class of machine as the other notes in this directory. Bench:
`geopackage/benches/filtered.rs`, criterion, 10 samples per point, 200,000
diagonal points with two attributes, spatially indexed, in a temp file.

The question, recorded by M3 with the design it never built: one thread walks
the RTree and hands candidate id blocks to workers, so the scan happens once
and no feature is returned twice. Is there enough to win?

## Figures

`candidates_only` is the RTree subquery stepped and counted, touching no
feature: the work the design keeps on one thread, so the serial floor.
`read_arrow_in` is the shipped single-threaded filtered read, drained.

| Selectivity | Rows | candidates_only | read_arrow_in | Floor share |
|---|---|---|---|---|
| 1% | 2,000 | 73 us | 597 us | 12% |
| 10% | 20,000 | 674 us | 5.77 ms | 12% |
| 50% | 100,000 | 3.67 ms | 42.4 ms | 9% |
| 100% | 200,000 | 7.38 ms | 138.6 ms | 5% |

Unfiltered references over the same layer:

| Read | Time |
|---|---|
| `read_arrow`, 1 thread | 46.5 ms (interval 42.5 to 49.5) |
| `read_arrow`, 4 threads | 21.9 ms |

## What the figures say

**The serial floor is not the obstacle.** At every selectivity the RTree scan
is 5% to 12% of the filtered read, so Amdahl's argument permits a several-fold
speedup from workers. The design's premise survives measurement.

**But the filtered path's single-threaded overhead dwarfs what threads would
recover.** At 100% selectivity the filtered read (138.6 ms) is three times
the unfiltered sequential read of the very same rows (46.5 ms). The
difference is the filtered path's own costs: the per-page `IN (SELECT id FROM
rtree ...)` lookups where the unfiltered read scans a rowid range, the exact
per-candidate envelope re-test, and the declined aggregate path. Threading
that read with 4 workers could at best approach 138.6 / 4 + overheads, still
short of what the *sequential* unfiltered read already does. Parallelising a
path that is doing three times the necessary per-row work parallelises the
waste.

**Where filtering earns its keep, latency is already interactive.** The
selectivities an interactive map issues (1% to 10%) come back in 0.6 ms to
5.8 ms. The threaded machinery opens a connection per worker and spawns
threads per read; on the unfiltered read that overhead is repaid across
46 ms of work, on a 6 ms read it is a large fraction of the whole.

## Decision

**Not built.** The threaded filtered read stays unimplemented, and the item
closes. Two conditions would reopen it, in order:

1. The single-threaded gap closes first: if the filtered read's 3x overhead
   against the unfiltered path is ever reduced (candidate id ranges instead
   of `IN` lookups, or the aggregate path learning to re-test), the
   arithmetic above changes and threads inherit less waste.
2. A profile then shows large-selectivity filtered reads bounding a real
   workload. Selectivity below about 10% never qualifies on these figures:
   it is already at interactive latency single-threaded.

The bench stays in the tree (`cargo bench -p geopackage --features arrow
--bench filtered`), so the figures can be reproduced when either condition
is met.
