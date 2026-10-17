# How to read values from a coverage

A tiled gridded coverage stores measurements, such as elevations, as TIFF or
16-bit PNG tiles. This guide shows you how to look up the value at a position,
calculate a tile's statistics, and check that a coverage contains no NaN or
infinite samples.

## Enable the feature

The decoders are off by default, because they add the `tiff` and `png`
dependencies, which you don't need if you only read coverages as bytes:

```console
$ cargo add geopackage --features coverage-samples
```

## Look up the value at a position

`Coverage::value_at` takes a zoom level and a position in the coverage's
spatial reference system, and returns the value there, with the tile's and the
coverage's scale and offset applied:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::{GeoPackage, Interpolation};

let gpkg = GeoPackage::open_read_only("dem.gpkg")?;
let coverage = gpkg.coverage("elevation")?;
let zoom = *coverage.zoom_levels().last().expect("a zoom level");

match coverage.value_at(zoom, 1250.0, 4875.0, Interpolation::Bilinear)? {
    Some(value) => println!("{value} {}", coverage.ancillary().uom.as_deref().unwrap_or("")),
    None => println!("no data here"),
}
# Ok(()) }
```

`None` means that there is no value: the position is outside the extent, the
coverage stores no tile there, or a sample the lookup needs is the coverage's
`data_null`. A bilinear lookup needs the four samples around the position, so
one null among them gives `None`, as does a missing tile next to the position.

## Look up many positions

Each call to `value_at` decodes the tiles it reads. For a profile along a line,
or any set of positions, open a sampler, which keeps the tiles it has decoded:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::{GeoPackage, Interpolation};
# let gpkg = GeoPackage::open_read_only("dem.gpkg")?;
# let coverage = gpkg.coverage("elevation")?;
let mut sampler = coverage.sampler(0)?;
for step in 0..100 {
    let x = 100.0 + f64::from(step) * 50.0;
    let height = sampler.value_at(x, 3000.0, Interpolation::Nearest)?;
    println!("{x}: {height:?}");
}
# Ok(()) }
```

## Where the samples are

The coverage's `grid_cell_encoding` says where in its cell each sample sits,
and the lookup follows it:

- `grid-value-is-center`, the default: the sample is at the centre of the cell.
- `grid-value-is-corner`: the sample is at the lower-left corner of the cell.
  The extension does not name the corner, and this library reads the
  lower-left, as NGA's implementation does.
- `grid-value-is-area`: the value holds across the whole cell, so a bilinear
  lookup returns the cell's value, as a nearest lookup does.

An encoding the extension does not define is read as the default.

## Write tiles with their statistics

`CoverageWriter::put_with_statistics` decodes a payload before it writes it,
refuses one that contains NaN or an infinity (Requirement 21), and records the
minimum, maximum, mean and standard deviation in the tile's ancillary row:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::core::tiles::TileCoord;
# use geopackage::{GeoPackage, TileAncillary};
# let gpkg = GeoPackage::open("dem.gpkg")?;
# let coverage = gpkg.coverage("elevation")?;
# let payload: Vec<u8> = Vec::new();
let mut writer = coverage.writer()?;
writer.put_with_statistics(TileCoord::new(0, 0, 0), &payload, &TileAncillary::defaults())?;
writer.commit()?;
# Ok(()) }
```

The statistics are of values, after the scale and offset, as the extension
specifies. Samples equal to `data_null` are left out, and the standard
deviation is that of the population. GDAL records its statistics in the same
way.

## Check a coverage's samples

With the feature enabled, `GeoPackage::validate` decodes every coverage tile
and reports tiles with a NaN or infinite sample as Requirement 21, and tiles
whose data does not decode as the requirement of their encoding. To check one
tile yourself, decode it and call `check_samples`:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::core::tiles::TileCoord;
# use geopackage::GeoPackage;
# let gpkg = GeoPackage::open_read_only("dem.gpkg")?;
# let coverage = gpkg.coverage("elevation")?;
if let Some(samples) = coverage.samples(TileCoord::new(0, 0, 0))? {
    coverage.check_samples(&samples)?;
}
# Ok(()) }
```

## Decode larger tiles

A tile's header states its size, and a decoder allocates for that size before
it reads the data. So a decode first compares the header with the tile's zoom
level, as GDAL does, and refuses a tile larger than the level declares. On top
of that, it refuses a tile of more than 4096 by 4096 samples, which is the
largest tile GDAL writes by default: these calls return `Error::Tile` with
`TileError::PayloadTooLarge` for it, and `validate` skips its samples.

If your coverages have larger tiles, raise the limit when you open the file.
Each sample costs eight bytes decoded:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::OpenOptions;

let gpkg = OpenOptions::new()
    .max_decoded_samples(8192 * 8192)
    .open_read_only("dem.gpkg")?;
# let _ = gpkg;
# Ok(()) }
```

## Decode without SQLite

The decode, the statistics and the lookup arithmetic are in
`geopackage-core`, under its own `coverage-samples` feature, so code that
reads tiles some other way can use them. You supply the bytes of a tile and
its ancillary rows; `SampleLookup` says which samples a position reads, and
you fetch them:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::core::coverage::{
    CoverageAncillary, Interpolation, SampleLookup, TileAncillary, decode_samples,
};
use geopackage::core::tiles::{TileMatrix, TileMatrixSet};

# let tile_data: Vec<u8> = Vec::new();
// One 256 by 256 tile over its extent, from your own reader.
let set = TileMatrixSet::new(3857, 0.0, 0.0, 256.0, 256.0);
let matrix = TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0);
let coverage = CoverageAncillary::new("elevation", "float");
let tile = TileAncillary::defaults();
let samples = decode_samples(&tile_data)?;

if let Some(lookup) =
    SampleLookup::new(&set, &matrix, &coverage, 100.0, 50.0, Interpolation::Bilinear)
{
    let value = lookup.value(|sample| {
        // This grid has one tile, so every sample is in it.
        let (_, column, row) = sample.tile(&matrix).expect("a positive tile size");
        Ok::<_, std::convert::Infallible>(samples.value(column, row, &coverage, &tile))
    })?;
    println!("{value:?}");
}
# Ok(()) }
```
