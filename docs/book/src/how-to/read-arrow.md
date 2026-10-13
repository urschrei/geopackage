# How to read a layer as Arrow record batches

The columnar path builds Arrow arrays directly from the statement's column
values, without creating a feature object for each row. This guide shows you
how to read a layer as `RecordBatch`es, filter the read, and pass the result to
the rest of the Arrow ecosystem.

## Enable the feature

The columnar paths are off by default, because they add the `arrow-array` and
`arrow-schema` dependencies, which you don't need if you only use the scalar
API:

```console
$ cargo add geopackage --features arrow
```

## Read the whole layer

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::GeoPackage;
use geopackage::arrow::ArrowReadOptions;

let gpkg = GeoPackage::open_read_only("roads.gpkg")?;
let layer = gpkg.layer("roads")?;

for batch in layer.read_arrow(ArrowReadOptions::default())? {
    let batch = batch?;
    println!("{} rows", batch.num_rows());
}
# Ok(()) }
```

Attribute columns follow the type mapping documented on the
[`arrow` module](https://docs.rs/geopackage/latest/geopackage/arrow/index.html).
The geometry column is WKB with the `geoarrow.wkb` extension name, and its
metadata includes the CRS as PROJJSON. Batches arrive in primary-key order,
whether or not the read is threaded.

## Filter the read

Three variants take filters. All three are single-threaded:

- To read the rows intersecting a box, use `read_arrow_in`:

  ```rust,no_run
  # fn main() -> Result<(), Box<dyn std::error::Error>> {
  # use geopackage::{BoundingBox, GeoPackage};
  # use geopackage::arrow::ArrowReadOptions;
  # let gpkg = GeoPackage::open_read_only("roads.gpkg")?;
  # let layer = gpkg.layer("roads")?;
  let bbox = BoundingBox::new(-7.0, 53.0, -6.0, 54.0);
  let batches = layer.read_arrow_in(bbox, ArrowReadOptions::default())?;
  # Ok(()) }
  ```

  It uses the RTree index if the layer has one, and a full scan if not. Either
  way, it re-tests every candidate against its true `f64` envelope. The
  candidate set is fixed when the read is opened, so a row inserted while you
  are still pulling batches is not returned.

- To filter on attributes, use `read_arrow_where`, which appends a raw SQL
  `WHERE` clause. The library trusts the clause and does not parse or sanitise
  it. Its placeholders are `?1` to `?N`, bound in slice order:

  ```rust,no_run
  # fn main() -> Result<(), Box<dyn std::error::Error>> {
  # use geopackage::{GeoPackage, ValueRef};
  # use geopackage::arrow::ArrowReadOptions;
  # let gpkg = GeoPackage::open_read_only("roads.gpkg")?;
  # let layer = gpkg.layer("roads")?;
  let batches = layer.read_arrow_where(
      "class = ?1",
      &[ValueRef::Text("motorway")],
      ArrowReadOptions::default(),
  )?;
  # Ok(()) }
  ```

  You can also use it to read a single row in columnar form: pass `fid = ?1`
  with the key as its parameter.

- To apply both at once, use `read_arrow_in_where`, which intersects the two.

## Tune the read

`ArrowReadOptions` controls three things:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::arrow::ArrowReadOptions;

let options = ArrowReadOptions::with_batch_size(16_384)
    .with_threads(1)
    .with_max_batch_bytes(256 * 1024 * 1024);
# Ok(()) }
```

`with_batch_size` creates a new set of options from the defaults. The other
two chain onto any options value.

- `batch_size` is rows per batch, 65,536 by default.
- `threads` defaults to `min(4, available parallelism)`. Set it to `1` to keep
  the read on your own thread. The read uses threads only when the database is
  a file, the primary key is dense, and there is more than one batch of rows.
  Otherwise it runs single-threaded rather than failing.
- `max_batch_bytes` limits the geometry bytes in one batch, and defaults to
  `min(INT32_MAX, RAM / 4)`. The geometry column's Arrow offsets are 32-bit, so
  2 GB is a hard ceiling. A batch that would cross the limit is emitted with
  fewer rows. Lower it for a layer of very large geometries, where the default
  multiplied by the thread count is more memory than you want in use at once.

## Hand the batches to something else

`ArrowBatches` implements `RecordBatchReader`, so you can pass it to anything
in the Arrow ecosystem that consumes one, such as a Parquet writer or the Arrow
C Data Interface:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::GeoPackage;
# use geopackage::arrow::ArrowReadOptions;
use arrow_array::RecordBatchReader;

# let gpkg = GeoPackage::open_read_only("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
let batches = layer.read_arrow(ArrowReadOptions::default())?;
let schema = batches.schema();
# Ok(()) }
```

## Copy a layer through Arrow

`Layer::arrow_schema` and `TableSchemaBuilder::from_arrow_schema` apply the
same type mapping in opposite directions, so you can copy a layer without
restating its schema:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::{GeoPackage, TableSchemaBuilder};
use geopackage::arrow::ArrowReadOptions;

let src = GeoPackage::open_read_only("roads.gpkg")?;
let roads = src.layer("roads")?;

let dst = GeoPackage::create("copy.gpkg")?;
let schema = roads.arrow_schema()?;
dst.create_layer(&TableSchemaBuilder::new("roads").from_arrow_schema(&schema)?)?;

let batches = roads.read_arrow(ArrowReadOptions::default())?;
dst.layer("roads")?.write_arrow(batches, 0)?;
# Ok(()) }
```

Here the geometry column is declared with its source type. A layer built from
an Arrow schema alone gets `GEOMETRY`, because the GeoArrow WKB encoding does
not record a specific type.

## Then

- Each batch is a separate query, paginated on the primary key, so a
  concurrent writer can change the table between batches. If you need one
  snapshot of the whole layer, open your own transaction on
  `GeoPackage::connection`. See
  [Transactions, and who commits](../explanation/transactions.md).
- API reference:
  [`Layer::read_arrow`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow),
  [`Layer::read_arrow_in`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow_in),
  [`Layer::read_arrow_where`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow_where),
  [`ArrowReadOptions`](https://docs.rs/geopackage/latest/geopackage/arrow/struct.ArrowReadOptions.html),
  and the [`arrow` module](https://docs.rs/geopackage/latest/geopackage/arrow/index.html)
  for the type mapping.
