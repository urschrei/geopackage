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

## Phase 1: the profile checker

New module `geopackage-core/src/coverage.rs`. Not inside `tiles.rs`: phase 2
adds ancillary DDL and model types that belong in the same module, and
`self_named_module_files` makes growing `tiles.rs` into a directory later a
churny rename.

- [ ] `SampleType` (`Float32`, `Unsigned(bits)`, `Signed(bits)`),
      `TiffCompression` (`None`, `Lzw`, `Deflate`), `CoverageTiff`
      (sample type, compression, width, height), all `#[non_exhaustive]` per C5.
- [ ] `coverage_tiff(bytes: &[u8]) -> Result<CoverageTiff, TileError>`, checking
      the tags below.
- [ ] Bounded parsing, since a `tile_data` BLOB is input this crate did not
      write: one pass, no recursion, nothing allocated from a file-declared
      count, entry count capped at the format's own `u16` bound, every offset
      bounds-checked against `bytes.len()` before use. Entries read from a
      `&[u8]` through `as_chunks::<12>()`, as the GPB envelope reader does.
      `get()` and `try_into()` throughout: `indexing_slicing` and `unwrap_used`
      are workspace lints.
- [ ] Both byte orders, chosen once from the header and threaded through as a
      reader pair rather than branched on per tag.
- [ ] One new `TileError` variant with the requirement number, as the tile
      matrix errors already include Requirement 45:
      `CoverageProfileViolation { requirement: u8, detail: String }`.
- [ ] C3: `probe` routes TIFF through this walker.
- [ ] C4: the doc comment states that Requirement 21 is not checked, and why.

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

- [ ] Hand-built headers as byte literals, little- and big-endian, one per
      violation: BigTIFF magic, a second IFD, `TileWidth` present,
      `SamplesPerPixel` 3, `BitsPerSample` absent/4/64, `SampleFormat` 3 with
      16 bits, JPEG compression. Each asserts the requirement number, not just
      that an error occurred.
- [ ] Truncation sweep: for a valid payload, `coverage_tiff(&bytes[..n])`
      errors rather than panics for every `n`.
- [ ] Fixture: `scripts/generate_fixtures.py` gains a float32 DEM through
      `gdal_translate -of GPKG -co TILE_FORMAT=TIFF`, committed as
      `geopackage/tests/fixtures/gdal_coverage.gpkg`. LZW, single strip,
      Float32: the centre of the profile, and the interop anchor.
- [ ] Corpus: `corpus_external.rs` already walks tiles one at a time, so the
      checker runs over every TIFF payload in the NGA and GDAL sample sets, and
      a violation there is reported rather than assumed absent.
- [ ] Fuzz: `fuzz_targets/tile_payload.rs` calls `coverage_tiff` on the same
      bytes. No panic, and when both succeed its width and height equal
      `probe`'s (which C3 makes a tautology, and the assertion guards the day it
      stops being one).

## Phase 1b: surface it

- [ ] `GeoPackage::validate()` gains a finding: for a table registered under
      `gpkg_2d_gridded_coverage`, every TIFF payload is checked and any
      violation reported with its requirement number. Severity follows the
      existing convention for "wrong but readable".
- [ ] `gpkg tiles info` describes a TIFF payload as `TIFF, float32, LZW,
      256x256` rather than `TIFF`.
- [ ] `gpkg validate` picks the finding up for free, being a printer for
      `validate()`.

## Phase 2: the extension proper (not scheduled)

Sketched so phase 1 is not orphaned, not planned here:
`gpkg_2d_gridded_coverage_ancillary` and `gpkg_2d_gridded_tile_ancillary` DDL
verbatim from the migrated spec source, the three `gpkg_extensions` rows
(scope `read-write`), a coverage model with `datatype`, `data_null` and the
two scale/offset pairs, Requirement 11's rule that a float coverage keeps the
default scale and offset, Requirement 13's PNG-16 alternative for integer
coverages, accepting TIFF on write, and the extension's ETS classes.
The `Extension::GriddedCoverage` doc comment cites 17-066r1 and should cite r2
when this lands.

## Acceptance criteria

1. [ ] Every TIFF payload in the fetched corpus and in the committed GDAL
   fixture is judged, and each judgement is either conformant or a reported
   violation naming its requirement, with no payload unchecked.
2. [ ] The fuzz target runs the checker without a panic or a timeout over a
   soak of the length M4's tile fuzzing used.
3. [ ] `gpkg validate` reports a violation this workspace synthesised (a
   deliberately non-conformant payload written through raw SQLite) and stays
   silent on the GDAL fixture.
4. [ ] No new dependency in `Cargo.toml`, and `geopackage-core` still decodes
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
