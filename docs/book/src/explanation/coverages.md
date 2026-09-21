# Tiled gridded coverages, and why they are not tile pyramids

A GeoPackage can hold two kinds of tiled raster. One is a basemap: PNG or JPEG
tiles that a renderer draws. The other is a *tiled gridded coverage* — a grid
of measurements, usually elevation, stored tile by tile in the same pyramid
structure. They look alike on disk and are almost nothing alike in use, and
this library keeps them apart on purpose.

## What the extension actually is

Coverages are an extension to the core standard, published separately as
[OGC 17-066r2](https://docs.ogc.org/is/17-066r2/17-066r2.html) and registered
in a file as `gpkg_2d_gridded_coverage`. It adds three things to a tile
pyramid:

- a different `gpkg_contents.data_type`, `2d-gridded-coverage` rather than
  `tiles`;
- `gpkg_2d_gridded_coverage_ancillary`, one row per coverage, saying what the
  samples *mean*: whether they are integers or floats, the scale and offset
  that carry a stored sample to a real value, the value that means "no data",
  the unit of measure;
- `gpkg_2d_gridded_tile_ancillary`, one row per **tile**, carrying that tile's
  own scale and offset and, optionally, its minimum, maximum, mean and
  standard deviation.

A tile's payload is a TIFF or a 16-bit greyscale PNG, and the extension
constrains it tightly: one sample per grid cell, 32-bit float or 8/16/32-bit
integer samples, LZW compression at most, one image per file, no internal
tiles.

## It is not a GeoTIFF

The most common assumption about coverages is that their TIFF payloads are
GeoTIFFs. They are not, and the distinction saves real work.

Georeferencing comes from `gpkg_tile_matrix_set` — the same extent and grid a
basemap pyramid uses — so the payload needs to carry none. The extension asks
for no `GeoKeyDirectory`, no `ModelPixelScale`, no `ModelTiepoint`. What it
asks for is a small profile of *baseline* TIFF, and every rule in it is a
statement about a tag rather than about a pixel.

That is why this library can check a coverage payload completely while owning
no image codec at all: `geopackage_core::coverage::coverage_tiff` walks the
first IFD, reads seven tags, and answers the encoding requirements from what
it finds. `coverage_png` does the same for `IHDR`. Neither decodes a sample.

## What that costs, stated plainly

One requirement is out of reach. Requirement 21 says every pixel in a tile
must hold a valid value and that NaN and Inf must not appear — which is a
statement about samples, and reaching them means decoding the strip. A payload
this library accepts is structurally conformant and says nothing about its
contents.

The same boundary shapes the write path. The four per-tile statistics are
computed from samples, so they are `None` unless you supply them:

```rust,no_run
# use geopackage::core::tiles::TileCoord;
# use geopackage::{GeoPackage, TileAncillary};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let gpkg = GeoPackage::open("dem.gpkg")?;
# let coverage = gpkg.coverage("elevation")?;
# let payload: Vec<u8> = Vec::new();
let mut writer = coverage.writer()?;
writer.put_with_ancillary(
    TileCoord::new(0, 0, 0),
    &payload,
    // Numbers from whatever decoded the samples; without them the row is
    // written with nulls, which is conformant and thinner than GDAL's output.
    &TileAncillary::defaults().with_statistics(0.5, 96.5, 48.53, 27.99),
)?;
writer.commit()?;
# Ok(()) }
```

## Why a separate handle

`GeoPackage::tiles` refuses a coverage, and `GeoPackage::coverage` refuses a
basemap. That is deliberate, and there are three reasons.

**Every tile needs a second row.** Requirement 10 says each tile in the user
table has a matching `gpkg_2d_gridded_tile_ancillary` row, and the normative
table definition has no `ON DELETE CASCADE`, so nothing maintains that pairing
for you. `CoverageWriter` writes both and deletes both. A `TilePyramid`
writer knows nothing about it, and a coverage reaching one would quietly
accumulate tiles with no rows.

**The payload rules are the opposite way round.** A tile pyramid may hold PNG,
JPEG or WebP and must not hold TIFF; a coverage must hold TIFF or 16-bit PNG
and nothing else. Both checks are strict, and neither is a special case of the
other.

**What you want back differs.** From a basemap you want bytes, to hand to a
decoder. From a coverage you want a *value*, which means the stored sample put
through two scale-and-offset pairs and compared against the null:

```rust,no_run
# use geopackage::core::tiles::TileCoord;
# use geopackage::GeoPackage;
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let gpkg = GeoPackage::open_read_only("dem.gpkg")?;
let coverage = gpkg.coverage("elevation")?;
let tile = coverage.tile_ancillary(TileCoord::new(0, 0, 0))?.unwrap();

// `sample` came from decoding the payload, which is your side of the line.
let sample = 1234.0;
if coverage.is_null(sample) {
    println!("no data here");
} else {
    println!("{} {}", coverage.value(&tile, sample),
             coverage.ancillary().uom.as_deref().unwrap_or(""));
}
# Ok(()) }
```

Note the asymmetry in that snippet: the null is compared against the *stored*
sample, untouched by either scale or offset, because the extension says the
scale and offset do not apply to it. Applying them first is an easy and silent
way to lose your no-data cells.

## Reading files other people wrote

As everywhere else in this library, reading is generous and writing is strict.
A coverage whose `datatype` column holds something the requirement does not
allow still opens, and reports `None` from `Coverage::datatype`. A payload
compressed with Deflate is read, though Requirement 15 asks for baseline TIFF
and Deflate is not part of it — and the same payload is refused on write.

What a file *is* wrong about, `GeoPackage::validate` will tell you, requirement
by requirement: ancillary rows that point nowhere, tiles with no row, a float
coverage that scales its samples, a payload that contradicts the `datatype` its
coverage declares. That last one is reported as an error rather than a warning,
because a reader honouring the ancillary row gets wrong numbers out of it.

One caveat worth knowing before you call it on a large file: this is the only
check in `validate` whose work scales with the file's size rather than with the
number of tables, because the encoding requirements are statements about
payloads and every payload has to be read to check them.
