# M6: tiled gridded coverage

Goal: the `gpkg_2d_gridded_coverage` extension (OGC 17-066r2), starting with
the half that needs no image codec. **Read-side validation first, write
later**: this crate can already open a coverage file that it did not write, and
read its tiles as bytes, but it cannot say whether those bytes conform. Phases
1 and 1b close that gap. Phase 2, which accepts TIFF on write, is a separate
decision and is not scheduled.

Not scheduled before M5's API freeze, and deliberately not a phase of M5: two
ancillary tables, a payload profile and its own ETS classes are a milestone's
worth of work, and M5 is full.

## Why this is live again

M4 rejected TIFF payloads on write because the extension was "under
revision" upstream, and M5 had a non-goal that called for one new assessment of
that status. It was, on 2026-09-12, and the revision is finished: the extension's own
repository was archived on 2026-08-04 with its text migrated into
[`spec/2d-gridded-coverage`](https://github.com/opengeospatial/geopackage/tree/master/spec/2d-gridded-coverage)
in the main spec repo, and 17-066r2 (version 1.1) is published. r2 is wider
than r1: it admits integer samples beside float32. The survey behind the
dependency question is in
[02-ecosystem.md](02-ecosystem.md#tiff-and-geotiff-assessed-2026-09-12-no-dependency-adopted);
in short, the coverage payload is **not** a GeoTIFF. It has no GeoKeys,
because the georeferencing is in `gpkg_tile_matrix_set`, so the missing GeoTIFF
writer in the ecosystem does not affect this work.

## Decisions settled before the work starts

### C1. No image codec, in core or out of it

Requirements 16 to 20 constrain tags, not pixels, so a walk of the first IFD
checks them: the same kind of header read that `geopackage_core::tiles::probe`
already does. That keeps M4's rule ("bytes in, bytes out, no decode in core"),
adds no dependency, and is all that a conformance check can assert. Decoding
samples is a question for phase 2 at the earliest, and `tiff` 0.11 is the
answer when it comes. The assessment gives the reasons, and the measurement
that its encoder is strip-only, which is what Requirement 20 requires and not a
gap.

### C2. Deflate: accepted on read, rejected on write

Requirement 18 says that LZW *MAY* be used, and does not say "only LZW". But
Requirement 15 says that the file conforms to baseline TIFF, and Deflate
(`Compression` 8, or 0x80B2 for the old code) is a TIFF extension, not
baseline. The two clauses do not agree. The crate resolves this with its
existing asymmetry, settled in M5 phase 1: a read accepts more than a write. So
a Deflate-compressed coverage tile validates on read, is named as such in
`CoverageTiff::compression`, and is rejected on write. The choice has little
practical effect: GDAL writes LZW (`gdalgeopackagerasterband.cpp` sets
`COMPRESS=LZW` on the GTiff driver, with a single strip for tiles up to
512x512), and its float-tile read driver list is `{"GTiff"}`.

### C3. One TIFF parser, not two

`probe` gets TIFF dimensions from `imagesize` today. Adding a second IFD walker
beside it leaves two parsers that can disagree on adversarial input, which the
fuzz target would eventually find and which would then have to be reconciled
anyway. So `probe` routes `TileFormat::Tiff` through this module's walker and
keeps `imagesize` for PNG, JPEG and WebP. This changes the error text `probe`
produces for a malformed TIFF, which no public contract covers.

### C4. Requirement 21 is out of reach and is documented as such

"All pixels in a tile of coverage data _SHALL_ be set with a valid component
value … Special floating point values such as NaN and Inf SHALL NOT be used"
cannot be checked without decoding samples. It is a documented limit of the
checker, in the style of M4's note that this crate's own tests, not the ETS,
cover `Tiles Encoding WebP`. It is not a silent omission.

### C5. The new types are `#[non_exhaustive]`; `TilePayload` is not extended

`TilePayload` does not have `#[non_exhaustive]`, so a new field is a breaking
change, and a coverage profile does not belong on the type that every PNG tile
also uses. The checker returns its own type instead. That type and the
sample-type enum are `#[non_exhaustive]` from the start, because r2 already
widened this profile once.

### C6. A coverage is its own thing, not a kind of tile pyramid

Settled 2026-09-20, after phase 1 raised the question. A coverage declares
`gpkg_contents.data_type` as `2d-gridded-coverage` (Requirement 5), not
`tiles`, and the spec gives it a separate data type for a reason: its tiles
contain measurements, not images, each tile needs a
`gpkg_2d_gridded_tile_ancillary` row (Requirement 10), and a reader needs
values with a scale, an offset and a null, not bytes for a decoder. So a
coverage gets its own handle, not a flag on `TilePyramid`.

What follows from it:

- `GeoPackage::coverage(name)` and `GeoPackage::coverages()`, returning a
  `Coverage` handle that contains the ancillary metadata (`datatype`,
  `data_null`, `scale`, `offset`, `precision`, `grid_cell_encoding`, `uom`).
  Internals shared with `TilePyramid` (`TileSql`, the cursor, the matrix
  model) are extracted rather than duplicated.
- **`tiles()` and `tile_pyramids()` do not change.** They stay about tile
  pyramids, still reject a coverage with `WrongDataType`, and still reject a
  TIFF payload on write. A caller who needs both makes two calls. That is the
  cost of this decision, and in exchange no coverage reaches a writer that does
  not know about its ancillary rows.
- `ContentsDataType` gains a `Coverage` variant. It is `#[non_exhaustive]`,
  so that is additive rather than breaking, but it *does* change the answer
  existing code gets: anything matching `Other("2d-gridded-coverage")` stops
  matching, including the coverage sweep phase 1 added to
  `corpus_external.rs`. That call site is the one to fix with the variant.
- The CLI gets `gpkg coverage info` rather than a widened `gpkg tiles info`,
  and `gpkg info`'s contents listing (which filters to
  `ContentsDataType::Tiles`) grows a coverage line.
- No C ABI consequence: `geopackage-ffi` does not expose `ContentsDataType`
  today, so a C surface for coverages is a later decision of its own.

## Phase 1: the profile checker

New module `geopackage-core/src/coverage.rs`. Not inside `tiles.rs`: phase 2
adds ancillary DDL and model types that belong in the same module, and
`self_named_module_files` makes growing `tiles.rs` into a directory later a
churny rename.

- [x] `SampleType` (`Float32`, `Unsigned(bits)`, `Signed(bits)`),
      `TiffCompression` (`None`, `Lzw`, `Deflate`), `CoverageTiff`
      (sample type, compression, width, height), all `#[non_exhaustive]` per C5.
- [x] `coverage_tiff(bytes: &[u8]) -> Result<CoverageTiff, TileError>`, checking
      the tags below.
- [x] Bounded parsing, since a `tile_data` BLOB is input this crate did not
      write: one pass, no recursion, nothing allocated from a file-declared
      count, entry count capped at the format's own `u16` bound, every offset
      bounds-checked against `bytes.len()` before use. Entries read from a
      `&[u8]` through `as_chunks::<12>()`, as the GPB envelope reader does.
      `get()` and `try_into()` throughout: `indexing_slicing` and `unwrap_used`
      are workspace lints. *(One addition to the plan: a tag with a value that
      does not fit its entry is unread, and the walk does not follow its
      offset, so the walk never dereferences a pointer from the file. Every tag
      that the profile constrains is a single number, so the result is the same
      as a full read, and a tag that cannot be read fails its own requirement
      and does not pass.)*
- [x] Both byte orders, chosen once from the header and threaded through as a
      reader pair rather than branched on per tag.
- [x] One new `TileError` variant with the requirement number, as the tile
      matrix errors already include Requirement 45:
      `CoverageProfileViolation { requirement: u8, detail: String }`.
- [x] C3: `probe` routes TIFF through this walker. *(The Requirement 15 check
      is in `coverage_tiff`, not in the walk, for a reason: `imagesize`
      classifies a BigTIFF as a TIFF, so the probe gives one to the walker, and
      the result must be "unreadable", not a coverage requirement that the
      probe does not check.)*
- [x] C4: the doc comment states that Requirement 21 is not checked, and why.

### What the walker checks, tag by tag

| Req | Check | Tag |
|---|---|---|
| 15 | `II`/`MM` byte order + magic 42; magic 43 (BigTIFF) is not baseline, reject | none |
| 19 | next-IFD offset is 0, and `SubIFDs` absent | 330 |
| 20 | `TileWidth`, `TileLength`, `TileOffsets`, `TileByteCounts` all absent | 322-325 |
| 16 | `SamplesPerPixel` = 1 (absent means 1, the baseline default) | 277 |
| 17 | `SampleFormat` in {1, 2, 3} (absent means 1); `BitsPerSample` in {8, 16, 32}, and 32 when `SampleFormat` is 3. The baseline default for `BitsPerSample` is **1**, so absent is a violation, not a pass | 339, 258 |
| 18 | `Compression` in {1, 5}, plus Deflate per C2 | 259 |
| none | `ImageWidth`/`ImageLength`, for the existing `TileMatrix::check_payload` cross-check | 256, 257 |

### Tests

- [x] Hand-built headers as byte literals, little- and big-endian, one per
      violation: BigTIFF magic, a second IFD, `TileWidth` present,
      `SamplesPerPixel` 3, `BitsPerSample` absent/4/64, `SampleFormat` 3 with
      16 bits, JPEG compression. Each asserts the requirement number, not just
      that an error occurred.
- [x] Truncation sweep: for a valid payload, `coverage_tiff(&bytes[..n])`
      errors rather than panics for every `n`.
- [x] Fixture: `scripts/generate_fixtures.py` gains a float32 DEM through
      `gdal_translate -of GPKG -co TILE_FORMAT=TIFF`, committed as
      `geopackage/tests/fixtures/gdal_coverage.gpkg`. LZW, single strip,
      Float32: the centre of the profile, and the interop reference. *(64 pixels
      square rather than 256, which keeps the fixture at 32 KB. Written by
      GDAL 3.8.4; the checker reads it back as `Float32`, `Lzw`, 64 by 64.
      Cross-checked off to the side against two more GDAL-written TIFFs that
      are not committed, a big-endian float32 and a big-endian uncompressed
      int16, so the byte-order and integer paths are read against a third
      party's encoder rather than only against this crate's test builder.)*
- [x] Corpus: `corpus_external.rs` already walks tiles one at a time, so the
      checker runs over every TIFF payload in the NGA and GDAL sample sets, and
      a violation there is reported rather than assumed absent. *(Not through
      the tile walk in the end, for the reason in "What phase 1 learned"
      below: the sweep reads a coverage's payloads through the SQL escape
      hatch and tallies `coverages`, `coverage_tiles` and `coverage_errors`
      beside the tile counts. Unrun here, since the fetched corpus is not part
      of a default test run.)*
- [x] Fuzz: `fuzz_targets/tile_payload.rs` calls `coverage_tiff` on the same
      bytes. No panic, and when both succeed its width and height equal those
      of `probe` (C3 makes this always true, and the assertion detects a change
      to C3). *(Made stronger during the work: a payload that the profile
      accepts must also probe. Otherwise a conforming coverage tile would fail
      the tile size check.)*

### Found while doing it

- [x] **A panic in `TileMatrixSet::tile_at`**, found by the first fuzz soak of
      the extended target and unrelated to the coverage work: a `TileMatrix`
      with `matrix_width` or `matrix_height` at zero or below reached
      `i64::clamp(0, width - 1)` with `min > max`, which panics. Requirements
      47 and 48 forbid that grid and `validate` rejects it, but `TileMatrix` is
      constructible directly, and one read from another writer's file has not
      been validated, so the panic was reachable through a public method on
      ordinary input. A grid with no columns contains no tile for any position,
      so `tile_at` now returns `None`, and `tile_range` does the same.
      Pre-existing: this path has been fuzzed since M4, and the corpus it was
      run against had never produced the values.
- [ ] **`Extension::GriddedCoverage` cites 17-066r1** in its doc comment and
      should cite r2. Left for phase 2d, where the citation starts being
      load-bearing.

### What phase 1 learned for phase 2

A coverage declares `gpkg_contents.data_type` as `2d-gridded-coverage`, not
`tiles`, so nothing here opened one: `GeoPackage::tiles` rejected it and
`tile_pyramids` did not list it, so its payloads were reachable only through
the SQL escape hatch. That left a question larger than the payload profile:
should a coverage be a `TilePyramid` with a flag, or a type of its own?
Settled in C6 and built in phase 2a: a type of its own.

## Phase 1b: surface it

**Folded into phase 2c**, and the reason is the order 2a settled: a validate
pass written before `Coverage` existed would have read payloads through the
SQL escape hatch and been rewritten the moment it did. The findings, their
severities and the cost question are planned in 2c below. What remains here
is the one piece that never needed a coverage handle:

- [ ] `gpkg tiles get --out` describes a TIFF payload it writes as
      `TIFF, float32, LZW, 256x256` rather than `TIFF`. It already prints what
      `probe` says, and a payload the profile can describe deserves the
      fuller line whichever table it came from.

*(Moved to 2c: the `validate()` findings, their severity split, and
`gpkg coverage info`, which needs the handle 2a built. `gpkg tiles info`
does not change (C6).)*

## Phase 2: the extension proper

Split into four. The order is read before
write before report: a writer whose output nothing here can read back is a
writer with no test, and a validator for files this crate cannot open is a
validator written twice.

### Phase 2a: the model and the read path

- [x] `Coverage`, its own handle per C6, with a private `TilePyramid` for
      the grid and the tile code. `GeoPackage::coverage(name)` and
      `GeoPackage::coverages()`; `tiles()` and `tile_pyramids()` do not change
      and still reject a coverage. The shared opener is `open_pyramid(name,
      data_type)`, `pub(crate)` in the tiles module, so nothing is duplicated
      and the public handles stay separate.
- [x] `CoverageAncillary` (Requirements 1, 7, 8, 9) and `TileAncillary`
      (Requirements 2, 10), read from the two ancillary tables. `datatype` is
      kept as text and parsed on demand, because a read does not reject a file:
      a value outside `integer`/`float` reads back as `None` rather than as an
      error.
- [x] `Error::NoCoverageAncillary`: `coverage()` rejects a coverage with no
      ancillary row, and does not open it with guessed defaults. Without the
      row there is no datatype, no scale or offset and no null, so a sample has
      no meaning.
- [x] `ContentsDataType::Coverage`, replacing the `Other("2d-gridded-coverage")`
      the catalogue used to report. The two CLI sites that filter on
      `ContentsDataType::Tiles` are unaffected, as C6 predicted, and the
      corpus sweep moved onto the new variant and the new handle.
- [x] Requirement 13's PNG alternative: `coverage_png` reads the `IHDR` header
      to the same depth the TIFF walk reads an IFD (16-bit, colour type 0, and
      nothing else), `coverage_payload` takes either encoding, and
      `CoveragePayload::sample_type` makes the two comparable.
- [x] `CoverageDatatype::check_payload`: Requirements 13 and 14, the rule that ties
      a payload to the `datatype` its coverage declares. The float direction is
      quoted from Requirement 14; the integer direction is a reading, and is
      documented as one, since Requirement 13 does not spell out the TIFF
      sample type and taking floating-point samples under `datatype = integer`
      as conforming would make the column meaningless.
- [x] `Coverage::value` and `Coverage::is_null`: the extension's scale/offset
      arithmetic on a sample decoded elsewhere, and the sentinel comparison
      that the scale and offset deliberately do not touch.
- [x] Tests: nine over the GDAL fixture (opening, the ancillary columns, the
      payload check in both directions, the per-tile row, the arithmetic, a
      cursor walk), and the rejections: a pyramid opened as a coverage, a
      coverage opened as a pyramid, a missing table, a coverage stripped of
      its ancillary row.

Not in 2a, and deliberately: nothing writes. A coverage reaches this crate
only if another implementation wrote it.

### Phase 2b: the write path

- [ ] DDL for both ancillary tables, verbatim from
      [annex-c](https://github.com/opengeospatial/geopackage/blob/master/spec/2d-gridded-coverage/annex-c.adoc),
      including its single-quoted table name. GDAL's differs (it folds the
      `CHECK` into the foreign-key constraint clause); ours follows the spec,
      and a read accepts both because a read does not look at the DDL.
- [ ] The three `gpkg_extensions` rows, with `definition` the r1 URL the r2
      spec source still prints and GDAL still writes
      (`COVERAGE_EXTENSION_DEFINITION`). Copied, not corrected.
- [ ] `CoverageBuilder` and `create_coverage`, with Requirement 11 enforced:
      a `float` coverage keeps both scale/offset pairs at their defaults.
- [ ] **Requirement 10 per tile.** Every tile insert needs its row id back
      (`RETURNING id`) and a second insert into
      `gpkg_2d_gridded_tile_ancillary`; every tile delete needs both rows
      gone, since the normative DDL has no `ON DELETE CASCADE`. Doubling the
      statements per tile will show in the tile-write benchmark, which is
      worth measuring rather than assuming. A Hegel property test over
      insert/delete sequences pins the pairing, as the RTree one pins the
      index.
- [ ] Accept TIFF on write for a coverage table only, after
      `Coverage::check_payload`. The ordinary tile path still rejects TIFF.
- [ ] Say plainly in the docs what a writer with no codec cannot do: the four
      per-tile statistics are left `NULL` unless the caller supplies them, and
      Requirement 21 is unenforceable.

### Phase 2c: validate and the CLI

- [ ] `validate()`: Requirements 7, 8, 10 and 11 as SQL joins (an ancillary
      row pointing at no coverage, a coverage with no row, a tile with no
      ancillary row, a float coverage whose scale or offset is not the
      default), plus the payload check over every tile. This is the pass
      phase 1b was going to write against raw SQL and can now write against
      `Coverage`.
- [ ] Severity split: a profile violation is a warning, because the header
      of the payload is correct; a payload that contradicts the `datatype` of
      the coverage is an error, because a reader that uses the ancillary row
      gets incorrect values.
- [ ] The cost question `validate()` has never had to answer before: this is
      the first check whose work scales with file size. Read whole payloads
      first, document it, and revisit with a benchmark rather than sampling
      tiles quietly.
- [ ] `gpkg coverage info`, and a coverage line in `gpkg info`'s contents
      listing.

### Phase 2d: interop and conformance

- [ ] GDAL round trip in `gdal_interop.rs`: a coverage this crate wrote, read
      back by `gdalinfo` with its elevations intact.
- [ ] The twelve abstract tests of
      [annex-a](https://github.com/opengeospatial/geopackage/blob/master/spec/2d-gridded-coverage/annex-a.adoc)
      implemented by hand. There is no ETS for this extension: `ets-gpkg12`
      validates the 1.2 core and tiles, and skips the rest. The abstract test
      suite is the nearest equivalent.
- [ ] `Extension::GriddedCoverage` cites r2 rather than r1.

## Acceptance criteria

These are phase 1's, and the milestone's as a whole; 2a adds none of its own,
because reading a coverage is only worth having once something reports on it
(2c) or writes one (2b).

1. [ ] Every TIFF payload in the fetched corpus and in the committed GDAL
   fixture is judged, and each judgement is either conformant or a reported
   violation naming its requirement, with no payload unchecked. *(Half
   met: the fixture is judged on every test run (through `Coverage` since
   2a, which checks the `datatype` as well as the profile), and the corpus
   sweep is written but has not been run against a fetched corpus.)*
2. [x] The fuzz target runs the checker without a panic or a timeout over a
   soak of the length M4's tile fuzzing used. *(Seven minutes and about six
   million executions, seeded with the GDAL fixture's own tile. The first soak
   found the `tile_at` panic above; the soak after the fix is clean.)*
3. [ ] `gpkg validate` reports a violation this workspace synthesised (a
   deliberately non-conformant payload written through raw SQLite) and stays
   silent on the GDAL fixture. *(Phase 1b.)*
4. [x] No new dependency in `Cargo.toml`, and `geopackage-core` still decodes
   no pixels.

## Explicit non-goals

- **Pixel access.** Returning samples needs `tiff`; off by default and in
  `geopackage` or a `geopackage-coverage` crate rather than `-core`, whose
  line is header inspection and never a decode. Not scheduled.
- **GeoTIFF ingest or export** (`gpkg tiles import dem.tif`): resampling and
  reprojection, not a codec question, and reprojection is D3's line. If it is
  ever wanted, the CLI shelling out to `gdal_translate -of GPKG` costs no
  library surface, as `gdal_interop.rs` already does.
- **BigTIFF, COG, overviews**: none appear in a `tile_data` BLOB.
- **NGA's tile scaling extension** and **vector tiles**, as M4 recorded.
