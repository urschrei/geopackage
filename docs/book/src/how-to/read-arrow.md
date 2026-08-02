# How to read a layer as Arrow record batches

The columnar path builds Arrow arrays straight from the statement's column
values, without going through a per-row feature object. This guide reads a
layer as `RecordBatch`es, filters that read, and hands the result to the rest
of the Arrow ecosystem.

## Enable the feature

The columnar paths are off by default, because they add the `arrow-array` and
`arrow-schema` dependencies that a caller using only the scalar API does not
need:

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
[`arrow` module](https://docs.rs/geopackage/latest/geopackage/arrow/index.html);
the geometry column is WKB with the `geoarrow.wkb` extension name, and its
metadata includes the CRS as PROJJSON. Batches arrive in primary-key order
whether the read is threaded or not.

## Filter the read

Three variants take filters, and all three are single-threaded:

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

  It uses the RTree index where the layer has one and a full scan where it
  does not, re-testing every candidate against its true `f64` envelope either
  way. The candidate set is fixed when the read is opened, so a row inserted
  while batches are still being pulled is not returned.

- To filter on attributes, use `read_arrow_where`, which appends a raw SQL
  `WHERE` clause. The clause is trusted from you and is not parsed or
  sanitised here; its placeholders are `?1` to `?N`, bound in slice order:

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

  This is also the columnar read for a single row: pass `fid = ?1` with the
  key as its parameter.

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

`with_batch_size` starts a fresh set of options from the default; the other
two chain onto whatever they are given.

- `batch_size` is rows per batch, 65,536 by default.
- `threads` defaults to `min(4, available parallelism)`. Set it to `1` for a
  read that touches no thread but yours. Threads are used only when the
  database is a file, the primary key is dense, and there is more than one
  batch of rows; the read is single-threaded rather than failing otherwise.
- `max_batch_bytes` bounds the geometry bytes one batch may contain, and
  defaults to `min(INT32_MAX, RAM / 4)`. The geometry column's Arrow offsets
  are 32-bit, so 2 GB is a hard ceiling; a batch that would cross the limit is
  emitted short. Lower it on a layer of very large geometries, where the
  default multiplied by the thread count is more memory than you want in
  flight.

## Hand the batches to something else

`ArrowBatches` implements `RecordBatchReader`, so it goes straight into
anything in the Arrow ecosystem that consumes one, including a Parquet writer
or the Arrow C Data Interface:

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

`Layer::arrow_schema` and `TableSchemaBuilder::from_arrow_schema` are the two
directions of the same type mapping, so a layer can be copied without its
schema being restated:

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

Note that the geometry column comes back declared as its source type here,
but a layer built from an Arrow schema alone gets `GEOMETRY`, because the
GeoArrow WKB encoding does not record a specific type.

## Then

- Each batch is a separate query, paginated on the primary key, so a
  concurrent writer can change the table between batches. Open your own
  transaction on `GeoPackage::connection` if you need one snapshot across the
  whole layer; see
  [Transactions, and who commits](../explanation/transactions.md).
- API reference:
  [`Layer::read_arrow`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow),
  [`Layer::read_arrow_in`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow_in),
  [`Layer::read_arrow_where`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow_where),
  [`ArrowReadOptions`](https://docs.rs/geopackage/latest/geopackage/arrow/struct.ArrowReadOptions.html),
  and the [`arrow` module](https://docs.rs/geopackage/latest/geopackage/arrow/index.html)
  for the type mapping.
