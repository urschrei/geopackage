# Design decisions

Decision record. Each entry: decision, rationale, consequences. Prior-art
lessons draw heavily on Hiroaki Yutani's (yutannihilation) write-up of building
`rusqlite-gpkg`:
["How it feels to write a GPKG library in 2026, in Rust"](https://dev.to/yutannihilation/how-it-feels-to-write-a-gpkg-library-in-2026-in-rust-52mg)
cited below as **[HY26]**. He hit most of the sharp edges first; where we
diverge from his choices it's deliberate and documented as such.

## D1. SQLite driver: rusqlite, sync core

**Decision.** rusqlite with `bundled` + `functions`. No sqlx. Async, if ever,
as a `spawn_blocking` wrapper crate.

**Rationale.** The RTree extension's triggers call `ST_IsEmpty`/`ST_MinX`/…,
functions SQLite does not have. Any connection that writes to an indexed table
must register them or writes fail ([HY26] hit this too; it is why geozero's
sqlx-based gpkg support cannot maintain spatial indexes –
[sqlx has no custom-function API](https://github.com/launchbadge/sqlx/discussions/1418)).
`bundled` guarantees `SQLITE_ENABLE_RTREE`; system-SQLite builds (feature)
must runtime-check `sqlite3_compileoption_used("ENABLE_RTREE")`.

**Consequences.** We own `ST_*` implementations, which means we own WKB
envelope computation (D6). rusqlite stays out of the public API except via
explicit escape hatches (`connection()`, `from_connection()`), so major-version
churn doesn't cascade.

## D2. Geometry API: geo-traits + georust wkb

**Decision.** Writes accept `impl geo_traits::GeometryTrait<T = f64>`; reads
yield a lazy GPB wrapper implementing the same trait, with `geo-types`
conversions behind a default feature. WKB body encode/decode delegates to
[georust `wkb`](https://github.com/georust/wkb).

**Rationale.** Z/M support without forcing conversions ([HY26] chose
geo-traits over geo-types for this reason; 3D was a hard requirement for his
use case). Same pattern as other georust crates (wkb, geoarrow-rs).

## D3. CRS policy: store faithfully, transform never

**Decision.** No PROJ dependency or coordinate transformation in the
core. `gpkg_spatial_ref_sys` rows for common EPSG codes come from vendored
definitions (see [02-ecosystem.md](02-ecosystem.md)); users can supply
arbitrary WKT; `gpkg_crs_wkt_1_1` (WKT2 + epoch) written when WKT2 is supplied.

**Rationale.** [HY26]'s observation: a GPKG **writer** is forced to
know CRS/WKT definitions ("actually requires PROJ if the writer wants to
support arbitrary SRIDs"), unlike GeoArrow, which delegates CRS interpretation
to consumers. We make our lives easy: ship WKT for known EPSG
codes, accept caller-provided WKT for everything else, and let
`proj`/`geodesy` remain composition points. A `srs_id → WKT` lookup failure is
a typed error telling the user to supply the definition.

**Under revision (issue #23).** The size argument for the vendored subset was an
estimate rather than a measurement, and the subset looks too small for real-world
files: a writer working outside it has to produce WKT itself, which is the burden
this decision was meant to avoid. M3 adds a second reason, since GeoArrow wants
PROJJSON for the geometry field's CRS and the subset cannot supply it for an
arbitrary code. The "transform never" half of this decision is not in question;
only how many definitions we carry, and in what formats.

## D4. Journal policy: interchange first

**Decision.** Default journal mode DELETE. WAL opt-in; on close (and on
`Drop`) we checkpoint and reset to DELETE so a handed-over `.gpkg` is a single
file.

**Rationale.** [HY26] and GDAL both flag WAL sidecars (`-wal`, `-shm`) as a
foot-gun for "give someone a .gpkg" and for non-local filesystems. WAL is a
performance win for concurrent read/write, so it stays available for
service-style use.

## D5. Wasm feature: design, but don't ship yet

**Decision.** Nothing in the API may assume `std::path` is the only way to
open a database (keep `from_connection` as the universal entry). But
browser/OPFS support is explicitly out of scope until after v0.2.

**Rationale.** [HY26] documents the pain: SQLite wants a real filesystem;
`sqlite-wasm-rs`'s OPFS/sahpool install is async while everything else is
sync; browser flows end up on `to_bytes`/`from_bytes` (rusqlite `serialize`).
rusqlite-gpkg already serves the browser niche; our FFI story (Arrow C
interface) is desktop/server-first. Revisit with `serialize`-based
`from_bytes`/`to_bytes` once the write path is stable. This also enables
in-memory workflows generally, which is worth having regardless of Wasm.

## D6. Envelopes: always write, compute on read only as fallback

**Decision.** Our writer always emits an XY (or XYZ when Z present) envelope
in the GPB header. Readers and `ST_*` functions prefer the header envelope;
envelope-less blobs (e.g. GDAL-written points) fall back to WKB traversal
(M1: full traversal via `wkb`; M0 ships point-only).

**Rationale.** Header envelopes make `ST_MinX` & co. O(1); the rtree triggers
call four of them per row, so this is a write-throughput decision, not a
nicety. Points get envelopes too (uniformity beats the 32-byte saving; GDAL
omits them for points, which is why the fallback must exist).

## D7. RTree correctness: version-aware triggers, never mixed

**Decision.** New indexes get the 1.4 set (`update5`/`update6`/`update7`).
On open we classify existing trigger generations; repair
(drop `update1`/`update3`, install 1.4 set) is an explicit user-invoked
operation, not automatic. Mixed generations are surfaced as a warning state.

**Rationale.** The pre-1.4 `update1` trigger corrupts indexes under UPSERT
(fixed by 1.4's rename-based detectability; see spec release notes and
[QGIS#36935](https://github.com/qgis/QGIS/issues/36935)). Automatic repair on
open would mutate files we were only asked to read.

## D8. Bulk-load path: build the index without the RTree module

**Decision.** Bulk inserts and `create_spatial_index()` on populated tables do
not fire per-row rtree inserts. The `(fid, envelope)` set is accumulated, the
tree is constructed in memory and the `rtree_%_node`/`_rowid`/`_parent` shadow
tables are written directly, in one transaction. Every build is gated (a
bijection and containment check against the accumulated set, plus
`rtreecheck`) and falls back to the triggered population on any anomaly.

**Rationale.** SQLite's rtree module has no bulk-load entry point: its only
write path is the per-row virtual-table update, so every insert descends the
tree and may split nodes. GDAL reached the same conclusion
([gdal#7614](https://github.com/OSGeo/gdal/issues/7614)).

**Superseded shape.** This decision originally specified GDAL's technique of
the time: build the rtree in an `ATTACH`ed scratch in-memory database and copy
its shadow tables across. That was implemented and then replaced, because
profiling showed the scratch build had become the dominant cost (4.61 s of a
7.26 s build at 1M points) for the same reason the original problem existed:
it still pushed every entry through the rtree module one row at a time.
Constructing the tree outright removed that, and removed the `ATTACH`, which
had required autocommit and so forced the index rebuild out of the caller's
transaction. Current GDAL no longer uses the scratch-database technique either;
it vendors a library that builds the tree in memory and writes the shadow
tables, the same category as this. See
[benchmarks/2026-07-24-packed-nodes.md](benchmarks/2026-07-24-packed-nodes.md)
and [benchmarks/2026-07-24-gdal-like-for-like.md](benchmarks/2026-07-24-gdal-like-for-like.md).

**Cost of the gate.** The gate is roughly 45% of a bulk build, and GDAL runs no
equivalent. It is kept because the tree is written by hand into a format SQLite
does not document as an interface; whether it should become optional is a
question for 1.0 (see 07-m5-extensions-and-1.0.md).

**When the path engages (refinement, issue #17).** `write_all` makes two
decisions, not one, and they need different evidence. Whether to drop the
triggers at all has to be settled before the first row is written, and is
settled from the write's size: from `Iterator::size_hint` where the hint can
settle it, and otherwise by buffering rows up to `bulk_threshold`, which is what
lets an iterator that does not know its own length reach the path at all.
Whether to then rebuild the index or add the new entries to it does *not* have to
be settled up front, so it is not: it waits until the rows are written, when the
number of new entries and the number already indexed are both exact, rather than
resting on the lower bound a size hint supplies. The append branch runs the
`_insert` trigger's own statement over the envelopes computed during encoding, so
it produces the tree the triggers would have, and is not gated (nothing is
hand-written into the shadow tables). Buffering is bounded by `bulk_threshold`
and never by the length of the input or the size of the table.

Both counts the deferred decision rests on, the index size and whether the table
was empty, are read after the trigger drop rather than before the transaction.
The drop is the transaction's first write statement and so the point at which
SQLite grants it the write lock; reading earlier left a window in which another
connection could commit a row in between. That matters most for the
table-was-empty test, which is what licenses reusing the encode-time envelopes as
the whole entry set: a row committed inside that window would be missing from the
rebuilt index, and the gate cannot catch it, because it checks the index against
that same set.

## D9. SQL is the query engine

**Decision.** We provide typed CRUD, bbox queries, and WHERE-clause
passthrough. We do not build a query DSL, and we expose the raw connection.

**Rationale.** GeoPackage *is* SQLite; wrapping SQL in Rust builders adds
surface without adding capability. This is also the escape valve for
everything we haven't implemented yet.

## D10. API shape: explicit container/layer/feature objects

**Decision.** `GeoPackage` → `Layer` → `Feature`/`Value`, schema declared via
builder (no derive macros in core; a `geopackage-derive` can be sugar later).

**Rationale.** Matches [HY26]'s landing point (`Gpkg`/`GpkgLayer`/
`GpkgFeature`/`Value`) and NGA's layered model; derive-macro-first was
gpkg-rs's design and it aged badly (schema locked at compile time).

## D11. Provenance and review

**Decision.** All normative SQL is copied verbatim from the spec source repo
with file-level citations. Non-trivial algorithms cite their origin (spec
requirement, GDAL issue, paper). LLM-assisted code is acceptable; unreviewed code is
not; every PR needs a human maintainer who can explain every line.

**Rationale.** reviewability *is* part of this crate's value
proposition against them unvetted LLM code.

## D12. Unsafe policy: forbidden everywhere except the FFI boundary

**Decision.** `unsafe_code = "forbid"` (and `missing_docs = "warn"`) are set
in the workspace lints table and inherited by every member crate via
`[lints] workspace = true`. The planned `geopackage-ffi` crate (M3) is the
sole intended exception: it will not inherit the workspace lints, `unsafe`
is confined to its C ABI / Arrow C Data Interface surface with the safety
contract documented on every block, and it gets sanitizer/miri gating in CI
before first release. `geopackage-core` and `geopackage` never gain
`unsafe`.

**Rationale.** The container and spec layers have no need for `unsafe`, and
a blanket forbid makes that checkable rather than aspirational. FFI
inherently requires `unsafe`; quarantining it in one crate keeps the audit
surface small and lets the rest of the workspace keep the hard guarantee.
The workspace-lints mechanism (rather than per-crate attributes) makes the
policy one declaration with one visible exception, and covers test and bench
targets too.
