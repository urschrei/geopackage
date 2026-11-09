# How to repair geometry and datetime encodings

Some writers store values in forms that GDAL reads but that the GeoPackage
standard does not give them:

- a geometry whose WKB gives Z or M by a high-bit flag in a type code
  (`0x80000003` for a polygon with Z), where a GeoPackage body uses the ISO
  code (`1003`);
- `DATETIME` text in another ISO 8601 form, such as `2017-08-01T00:00:00.0Z`,
  where the standard gives `2017-08-01T00:00:00.000Z`.

This library reads both, but by default it rejects the second, and a reader
that follows the standard can reject either. `gpkg validate` reports them as
`NonIsoWkbTypeCode` and `NonStandardDateTime`. This guide shows you how to
rewrite them in the standard form.

Repair is never automatic, and it changes the file in place. Work on a copy if
you need to keep the original.

## From the shell

`gpkg repair` makes both repairs, as well as the spatial index repair, on
every feature layer and attribute table that needs them:

```console
$ gpkg repair boundaries.gpkg
boundaries: 9071 geometries rewritten with ISO WKB type codes
boundaries: 136065 DATETIME values rewritten in the GeoPackage form
```

To repair one layer or attribute table, add its name:

```console
$ gpkg repair boundaries.gpkg boundaries
```

## From QGIS

With the GeoPackage toolkit plugin installed, run **Repair geometry and
datetime encodings** from the Processing toolbox or the plugin's menu. Leave
**Layer or table** empty to repair every table. The report has one row per
table, with how many geometries and `DATETIME` values were rewritten.

**Validate GeoPackage** names the algorithm that repairs each finding in its
`plugin_algorithm` column.

## From Rust

Open the file read-write with the lenient open, then call the two repairs on
each layer. Both return how many values they rewrote.

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::GeoPackage;

let gpkg = GeoPackage::open_lenient("boundaries.gpkg")?;

for layer in gpkg.layers()? {
    let geometries = layer.repair_wkb_type_codes()?;
    let datetimes = layer.repair_datetimes()?;
    println!("{}: {geometries} geometries, {datetimes} datetimes", layer.table_name());
}
# Ok(()) }
```

An attribute table can have `DATETIME` columns too. Open it with
`GeoPackage::attributes` and call `repair_datetimes` on it.

Each repair runs in one transaction, and writes only the rows that need it.
The rewritten geometries keep their coordinates and SRS id, and get an
envelope in their header. The RTree triggers keep the spatial index in step.

## What the repairs leave alone

- A geometry whose header or body cannot be read. `gpkg validate` reports it
  as `UnreadableGeometry`; its data has to come from the tool that wrote it.
- A WKB type code with the EWKB SRID flag, which this library does not read.
- `DATETIME` text that the lenient parser does not read, or that has a part
  smaller than a millisecond, which the standard form would lose.
- `DATE` columns, which have only one form.

Run `gpkg validate` afterwards to see anything that is left.

## Then

- [How to validate a GeoPackage and act on the findings](validate.md)
- [How to repair a file's spatial indexes](repair-spatial-indexes.md)
- API reference:
  [`Layer::repair_wkb_type_codes`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.repair_wkb_type_codes),
  [`Layer::repair_datetimes`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.repair_datetimes),
  [`curve::to_iso_wkb`](https://docs.rs/geopackage-core/latest/geopackage_core/curve/fn.to_iso_wkb.html).
