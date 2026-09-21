# Tiled gridded coverages, and why they are not tile pyramids

A GeoPackage can store two kinds of tiled raster. One is a basemap: PNG or JPEG
tiles that a renderer draws. The other is a *tiled gridded coverage*: a grid of
measurements, usually elevation, stored tile by tile in the same pyramid
structure. The two look similar on disk, but their uses are different, so this
library keeps them separate.

## What the extension adds

Coverages are an extension to the core standard, published separately as
[OGC 17-066r2](https://docs.ogc.org/is/17-066r2/17-066r2.html) and registered
in a file as `gpkg_2d_gridded_coverage`. The extension adds three things to a
tile pyramid:

- a different `gpkg_contents.data_type`: `2d-gridded-coverage`, not `tiles`;
- `gpkg_2d_gridded_coverage_ancillary`, one row for each coverage, which
  states what the samples *mean*: integer or float, the scale and offset that
  convert a stored sample to a value, the value for "no data", and the unit of
  measure;
- `gpkg_2d_gridded_tile_ancillary`, one row for each **tile**, with the scale
  and offset of that tile and, optionally, its minimum, maximum, mean and
  standard deviation.

The payload of a tile is a TIFF or a 16-bit greyscale PNG, and the extension
constrains it closely: one sample per grid cell, 32-bit float or 8, 16 or
32-bit integer samples, no compression or LZW compression, one image per file,
and no internal tiles.

## A coverage payload is not a GeoTIFF

A common assumption is that the TIFF payloads of a coverage are GeoTIFFs. They
are not, and this has a practical consequence.

The georeferencing is in `gpkg_tile_matrix_set`, the same extent and grid that
a basemap pyramid uses, so the payload does not need any. The extension does
not use `GeoKeyDirectory`, `ModelPixelScale` or `ModelTiepoint`. It specifies
a small profile of *baseline* TIFF, and each rule in the profile is a
statement about a tag, not about a pixel.

For this reason, this library can check a coverage payload completely without
an image codec. `geopackage_core::coverage::coverage_tiff` walks the first IFD,
reads seven tags, and checks the encoding requirements against them.
`coverage_png` does the same for `IHDR`. Neither function decodes a sample.

## The limits of a header check

This library cannot check one requirement. Requirement 21 says that every pixel
in a tile must have a valid value, and that NaN and Inf must not occur. That is
a statement about samples, and a check of the samples needs a decoder. A
payload that this library accepts conforms in structure only, and the check
says nothing about its contents.

The same limit applies to the write path. The four per-tile statistics come
from the samples, so they are `None` unless you supply them:

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
    // Values from the code that decoded the samples. Without them, the row has
    // nulls. That conforms, but the row has less data than a row from GDAL.
    &TileAncillary::defaults().with_statistics(0.5, 96.5, 48.53, 27.99),
)?;
writer.commit()?;
# Ok(()) }
```

## Why a coverage has a separate handle

`GeoPackage::tiles` rejects a coverage, and `GeoPackage::coverage` rejects a
basemap. There are three reasons.

**Every tile needs a second row.** Requirement 10 specifies a matching
`gpkg_2d_gridded_tile_ancillary` row for each tile in the user table. The
normative table definition has no `ON DELETE CASCADE`, so the database does not
keep the rows paired. `CoverageWriter` writes both rows and deletes both rows.
A `TilePyramid` writer does not know about the second row, and a coverage
written through it would collect tiles without rows.

**The payload rules are opposites.** A tile pyramid can store PNG, JPEG or
WebP, and must not store TIFF. A coverage must store TIFF or 16-bit PNG, and
nothing else. Both checks are strict, and neither is a special case of the
other.

**The result of a read is different.** From a basemap, you need bytes for a
decoder. From a coverage, you need a *value*: the stored sample, converted by
two scale and offset pairs, and compared with the null:

```rust,no_run
# use geopackage::core::tiles::TileCoord;
# use geopackage::GeoPackage;
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let gpkg = GeoPackage::open_read_only("dem.gpkg")?;
let coverage = gpkg.coverage("elevation")?;
let tile = coverage.tile_ancillary(TileCoord::new(0, 0, 0))?.unwrap();

// `sample` comes from a decode of the payload, which your code does.
let sample = 1234.0;
if coverage.is_null(sample) {
    println!("no data here");
} else {
    println!("{} {}", coverage.value(&tile, sample),
             coverage.ancillary().uom.as_deref().unwrap_or(""));
}
# Ok(()) }
```

In this example, the null is compared with the *stored* sample, before the
scale and offset apply, because the extension says that the scale and offset do
not apply to the null. If you apply them first, you lose your no-data cells,
and nothing reports an error.

## Files from other writers

As in all of this library, a read accepts more than a write. A coverage with a
`datatype` value that the requirement does not permit still opens, and
`Coverage::datatype` returns `None`. A read accepts a payload with Deflate
compression, although Requirement 15 specifies baseline TIFF and Deflate is not
part of it. A write rejects the same payload.

`GeoPackage::validate` reports the defects in a file, requirement by
requirement: ancillary rows that point to no tile, tiles without a row, a float
coverage that scales its samples, and a payload that contradicts the `datatype`
of its coverage. The last is an error, not a warning, because a reader that
uses the ancillary row gets incorrect values.

Before you call `validate` on a large file, note that the coverage check is the
only check with work that scales with the size of the file, not with the number
of tables. The encoding requirements are statements about payloads, so the
check must read every payload.
