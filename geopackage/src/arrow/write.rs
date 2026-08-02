use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, DataType, Schema, TimeUnit};
use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::types::{ColumnType, GeometryType};

use crate::{Error, Layer, Result};

use super::schema::{EXTENSION_METADATA_KEY, EXTENSION_NAME_KEY, GEOARROW_WKB, epsg_code};

/// Where each of a layer's columns sits in a batch, shared by all its rows.
struct RowLayout {
    fid: Option<usize>,
    geometry: Option<usize>,
    /// Batch column index for each of the layer's value columns, in order.
    /// `None` for a column the batch does not include, which is written as
    /// NULL.
    values: Vec<Option<usize>>,
}

/// One row of a [`RecordBatch`], as a view rather than a copy.
///
/// Holding the batch and an index, rather than owned values, is what lets the
/// write path bind strings and blobs straight out of the Arrow arrays. Both
/// handles are `Arc`, so a row costs two reference-count bumps where owning its
/// values cost an allocation per text cell plus a copy of the geometry.
struct ArrowRow {
    batch: Arc<RecordBatch>,
    layout: Arc<RowLayout>,
    row: usize,
}

/// A row, or the failure that stopped one being made.
///
/// Batches are taken apart lazily, so an error has to travel with the rows
/// rather than out of the side of the iterator. Writing one of these fails the
/// write, which rolls the transaction back exactly as any other write error
/// does.
enum ArrowRowResult {
    Row(ArrowRow),
    Failed(Error),
}

impl crate::writer::WritableRow for ArrowRowResult {
    fn write(self, writer: &mut crate::FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)> {
        match self {
            Self::Row(row) => row.write(writer),
            Self::Failed(error) => Err(error),
        }
    }
}

impl crate::writer::WritableRow for ArrowRow {
    fn write(self, writer: &mut crate::FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)> {
        let fid = match self
            .layout
            .fid
            .and_then(|index| self.batch.columns().get(index))
        {
            Some(column) => read_i64(column, self.row)?,
            None => None,
        };

        let mut values = Vec::with_capacity(self.layout.values.len());
        for (position, index) in self.layout.values.iter().enumerate() {
            let bound = match index.and_then(|index| self.batch.columns().get(index)) {
                Some(column) => bind_value(column, self.row, position, &self.batch)?,
                None => rusqlite::types::ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Null),
            };
            values.push(bound);
        }

        let geometry = self
            .layout
            .geometry
            .and_then(|index| self.batch.columns().get(index));
        match geometry {
            Some(column) if !column.is_null(self.row) => {
                let wkb = binary_at(column, self.row)?;
                writer.insert_wkb_bound(fid, wkb, &values)
            }
            _ => writer.insert_row_bound(fid, &values).map(|fid| (fid, None)),
        }
    }
}

impl Layer<'_> {
    /// Writes Arrow [`RecordBatch`]es into this layer.
    ///
    /// The columnar counterpart of [`crate::Layer::write_all`], and it shares
    /// that path: batching, the bulk spatial-index decision and the single
    /// transaction all behave identically. What differs is that a geometry
    /// arrives as WKB and stays as WKB, gaining a GPB header rather than being
    /// parsed into a geometry object and serialised again.
    ///
    /// The reader's schema must name columns this layer has. Extra columns in
    /// the batch are an error rather than being ignored, since silently dropping
    /// data a caller asked to write is worse than rejecting it. A column of the
    /// layer that the batch does not name is left to its default.
    ///
    /// A [`RecordBatchReader`](arrow_array::RecordBatchReader) does not say how many rows it will produce, which
    /// is the case the bulk index path buffers for rather than trusting a size
    /// hint (issue #17).
    ///
    /// Returns the assigned feature ids, in the order the rows were written.
    ///
    /// # Errors
    ///
    /// [`Error::ArrowValueMismatch`] for a column whose Arrow type does not fit
    /// the layer's declared type, [`Error`] for the write itself, and any error
    /// the reader yields.
    pub fn write_arrow<R>(&self, batches: R, batch_size: usize) -> Result<Vec<i64>>
    where
        R: IntoIterator<Item = std::result::Result<RecordBatch, ArrowError>>,
    {
        self.write_arrow_with(batches, batch_size, crate::BulkIndexOptions::default())
    }

    /// [`Self::write_arrow`] with an explicit [`crate::BulkIndexOptions`].
    ///
    /// # Errors
    ///
    /// As [`Self::write_arrow`].
    pub fn write_arrow_with<R>(
        &self,
        batches: R,
        batch_size: usize,
        options: crate::BulkIndexOptions,
    ) -> Result<Vec<i64>>
    where
        R: IntoIterator<Item = std::result::Result<RecordBatch, ArrowError>>,
    {
        let geometry_column = self.geometry_column().map(|g| g.column_name.clone());
        // The layer's value columns already exclude both the geometry and the
        // primary key, which this path binds through its own arguments.
        let value_columns: Vec<String> = self
            .value_columns()
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let primary_key = self.primary_key_column().map(str::to_owned);

        // One batch is taken apart at a time and the rows are handed on lazily,
        // so peak memory is a batch rather than the whole input, and the write
        // path sees the unsized source it buffers to size up (issue #17).
        // Collecting here would undo both.
        let rows = batches.into_iter().flat_map(move |batch| {
            let taken = batch.map_err(Error::Arrow).and_then(|batch| {
                let layout = layout_of(
                    &batch,
                    primary_key.as_deref(),
                    geometry_column.as_deref(),
                    &value_columns,
                )?;
                Ok((Arc::new(batch), Arc::new(layout)))
            });
            // Split into the two arms rather than collecting either, so a
            // batch's rows are still built one at a time. The arms are chained
            // instead of matched so both are the same iterator type: exactly one
            // of them ever yields.
            //
            // A failure travels as a row of its own, so it reaches the write
            // path and rolls the transaction back rather than needing a second
            // channel out of the iterator.
            let (batch, error) = match taken {
                Ok(batch) => (Some(batch), None),
                Err(error) => (None, Some(error)),
            };
            batch
                .into_iter()
                .flat_map(|(batch, layout)| {
                    (0..batch.num_rows()).map(move |row| {
                        ArrowRowResult::Row(ArrowRow {
                            batch: Arc::clone(&batch),
                            layout: Arc::clone(&layout),
                            row,
                        })
                    })
                })
                .chain(error.into_iter().map(ArrowRowResult::Failed))
        });
        self.write_all_impl(rows, batch_size, options, crate::bulk::no_fault)
    }
}

/// Work out where each of the layer's columns sits in this batch.
///
/// The layout is computed once per batch and shared, so a row stores two `Arc`
/// handles rather than a copy of anything.
fn layout_of(
    batch: &RecordBatch,
    primary_key: Option<&str>,
    geometry: Option<&str>,
    value_columns: &[String],
) -> Result<RowLayout> {
    let schema = batch.schema();
    for field in schema.fields() {
        let known = Some(field.name().as_str()) == primary_key
            || Some(field.name().as_str()) == geometry
            || value_columns.iter().any(|name| name == field.name());
        if !known {
            return Err(Error::NoSuchColumn {
                table_name: String::new(),
                column_name: field.name().clone(),
            });
        }
    }

    let index_of = |name: &str| schema.fields().iter().position(|f| f.name() == name);
    Ok(RowLayout {
        fid: primary_key.and_then(index_of),
        geometry: geometry.and_then(index_of),
        values: value_columns.iter().map(|name| index_of(name)).collect(),
    })
}

/// The bytes of a binary cell, borrowed from the array.
fn binary_at(column: &ArrayRef, row: usize) -> Result<&[u8]> {
    if let Some(binary) = column.as_binary_opt::<i32>() {
        return Ok(binary.value(row));
    }
    if let Some(binary) = column.as_binary_opt::<i64>() {
        return Ok(binary.value(row));
    }
    Err(Error::ArrowValueMismatch {
        column: String::new(),
        expected: "Binary or LargeBinary",
        found: "another Arrow type",
    })
}

/// Read one `Int64` cell, for the feature id.
fn read_i64(column: &ArrayRef, row: usize) -> Result<Option<i64>> {
    if column.is_null(row) {
        return Ok(None);
    }
    let values = column
        .as_primitive_opt::<arrow_array::types::Int64Type>()
        .ok_or_else(|| Error::ArrowValueMismatch {
            column: String::new(),
            expected: "Int64",
            found: "other",
        })?;
    Ok(Some(values.value(row)))
}

/// Bind one attribute cell, borrowing from the array wherever that is possible.
///
/// The inverse of the read path's mapping, and deliberately narrower: it accepts
/// what [`Layer::arrow_schema`] produces, so a round trip works, plus the
/// narrower integer and float widths another producer is likely to emit.
///
/// Strings and blobs are bound as slices into the Arrow buffers, which is the
/// point of this function: they are already contiguous there, so copying them
/// into a `Value` first would allocate once per cell for nothing. `DATE` and
/// `DATETIME` are the exception, because a GeoPackage stores them as text and
/// the text has to be produced.
fn bind_value<'a>(
    column: &'a ArrayRef,
    row: usize,
    position: usize,
    batch: &RecordBatch,
) -> Result<rusqlite::types::ToSqlOutput<'a>> {
    use arrow_array::types::{
        Date32Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
        TimestampMicrosecondType, TimestampMillisecondType,
    };
    use rusqlite::types::{ToSqlOutput, Value as SqlV, ValueRef};

    let borrowed = |value: ValueRef<'a>| Ok(ToSqlOutput::Borrowed(value));
    let owned = |value: SqlV| Ok(ToSqlOutput::Owned(value));

    if column.is_null(row) {
        return borrowed(ValueRef::Null);
    }
    // Only built when something is wrong, so the happy path does not pay for
    // naming the column.
    let name = || {
        batch
            .schema()
            .fields()
            .get(position)
            .map(|field| field.name().clone())
            .unwrap_or_default()
    };
    let mismatch = |expected: &'static str| Error::ArrowValueMismatch {
        column: name(),
        expected,
        found: "an array of another type",
    };
    let out_of_range = |source| Error::InvalidDateTimeValue {
        column: name(),
        text: "an Arrow date or timestamp outside the representable range".to_owned(),
        source,
    };

    match column.data_type() {
        DataType::Boolean => owned(SqlV::Integer(i64::from(
            column
                .as_boolean_opt()
                .ok_or_else(|| mismatch("Boolean"))?
                .value(row),
        ))),
        DataType::Int8 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<Int8Type>()
                .ok_or_else(|| mismatch("Int8"))?
                .value(row),
        ))),
        DataType::Int16 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<Int16Type>()
                .ok_or_else(|| mismatch("Int16"))?
                .value(row),
        ))),
        DataType::Int32 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<Int32Type>()
                .ok_or_else(|| mismatch("Int32"))?
                .value(row),
        ))),
        DataType::Int64 => borrowed(ValueRef::Integer(
            column
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(|| mismatch("Int64"))?
                .value(row),
        )),
        DataType::Float32 => owned(SqlV::Real(f64::from(
            column
                .as_primitive_opt::<Float32Type>()
                .ok_or_else(|| mismatch("Float32"))?
                .value(row),
        ))),
        DataType::Float64 => borrowed(ValueRef::Real(
            column
                .as_primitive_opt::<Float64Type>()
                .ok_or_else(|| mismatch("Float64"))?
                .value(row),
        )),
        DataType::Utf8 => borrowed(ValueRef::Text(
            column
                .as_string_opt::<i32>()
                .ok_or_else(|| mismatch("Utf8"))?
                .value(row)
                .as_bytes(),
        )),
        DataType::LargeUtf8 => borrowed(ValueRef::Text(
            column
                .as_string_opt::<i64>()
                .ok_or_else(|| mismatch("LargeUtf8"))?
                .value(row)
                .as_bytes(),
        )),
        DataType::Binary => borrowed(ValueRef::Blob(
            column
                .as_binary_opt::<i32>()
                .ok_or_else(|| mismatch("Binary"))?
                .value(row),
        )),
        DataType::LargeBinary => borrowed(ValueRef::Blob(
            column
                .as_binary_opt::<i64>()
                .ok_or_else(|| mismatch("LargeBinary"))?
                .value(row),
        )),
        DataType::Date32 => owned(SqlV::Text(
            Date::from_days_since_epoch(
                column
                    .as_primitive_opt::<Date32Type>()
                    .ok_or_else(|| mismatch("Date32"))?
                    .value(row),
            )
            .map_err(out_of_range)?
            .to_string(),
        )),
        DataType::Timestamp(TimeUnit::Microsecond, _) => owned(SqlV::Text(
            DateTime::from_micros_since_epoch(
                column
                    .as_primitive_opt::<TimestampMicrosecondType>()
                    .ok_or_else(|| mismatch("Timestamp"))?
                    .value(row),
            )
            .map_err(out_of_range)?
            .to_string(),
        )),
        DataType::Timestamp(TimeUnit::Millisecond, _) => owned(SqlV::Text(
            DateTime::from_micros_since_epoch(
                column
                    .as_primitive_opt::<TimestampMillisecondType>()
                    .ok_or_else(|| mismatch("Timestamp"))?
                    .value(row)
                    .saturating_mul(1_000),
            )
            .map_err(out_of_range)?
            .to_string(),
        )),
        other => Err(Error::UnsupportedArrowType {
            data_type: other.to_string(),
        }),
    }
}

impl crate::TableSchemaBuilder {
    /// Derive a layer definition from an Arrow schema.
    ///
    /// The inverse of [`Layer::arrow_schema`], for creating a layer to receive
    /// [`Layer::write_arrow`]. Everything the builder normally takes can still
    /// be overridden afterwards.
    ///
    /// The mapping is the read mapping run backwards, with three things worth
    /// knowing:
    ///
    /// - **Integer widths are honoured here but not on the way out.** `Int8`
    ///   becomes `TINYINT`, `Int16` `SMALLINT`, `Int32` `MEDIUMINT`, `Int64`
    ///   `INTEGER`. Reading collapses all four to `Int64`, because SQLite does
    ///   not enforce a declared width, so a layer round-tripped through Arrow
    ///   comes back with every integer column widened to `INTEGER`. The data is
    ///   unchanged; the declared type is not.
    /// - **The geometry column is found by its `geoarrow.wkb` extension name**,
    ///   not by position or by name. Its declared type is `GEOMETRY`, which
    ///   accepts any geometry, because WKB does not say what it will contain.
    ///   The SRS comes from the field's CRS metadata when that is an
    ///   `EPSG:<code>` authority code, and is otherwise `0`, the spec's
    ///   undefined value.
    /// - **A field named as the primary key is skipped**, not made an attribute
    ///   column, since the builder creates the key itself. Call
    ///   [`crate::TableSchemaBuilder::primary_key`] before this if the key is
    ///   not named `fid`.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedArrowType`] for a field whose type has no GeoPackage
    /// equivalent.
    pub fn from_arrow_schema(self, schema: &Schema) -> Result<Self> {
        let mut builder = self;
        for field in schema.fields() {
            if *field.name() == builder.primary_key_name() {
                continue;
            }
            if field.metadata().get(EXTENSION_NAME_KEY).map(String::as_str) == Some(GEOARROW_WKB) {
                let srs_id = field
                    .metadata()
                    .get(EXTENSION_METADATA_KEY)
                    .and_then(|json| epsg_code(json))
                    .unwrap_or(0);
                builder = builder.geometry(
                    crate::GeometrySpec::new(GeometryType::Geometry, srs_id)
                        .column_name(field.name()),
                );
                continue;
            }
            let column_type = column_type_for(field.data_type())?;
            let mut column = crate::ColumnSpec::new(field.name(), column_type);
            if !field.is_nullable() {
                column = column.not_null();
            }
            builder = builder.column(column);
        }
        Ok(builder)
    }
}

/// The GeoPackage column type for an Arrow type, the inverse of the mapping in
/// the [module documentation](super).
fn column_type_for(data_type: &DataType) -> Result<ColumnType> {
    Ok(match data_type {
        DataType::Boolean => ColumnType::Boolean,
        DataType::Int8 | DataType::UInt8 => ColumnType::TinyInt,
        DataType::Int16 | DataType::UInt16 => ColumnType::SmallInt,
        DataType::Int32 | DataType::UInt32 => ColumnType::MediumInt,
        DataType::Int64 | DataType::UInt64 => ColumnType::Integer,
        DataType::Float32 => ColumnType::Float,
        DataType::Float64 => ColumnType::Double,
        DataType::Utf8 | DataType::LargeUtf8 => ColumnType::Text(None),
        DataType::Binary | DataType::LargeBinary => ColumnType::Blob(None),
        DataType::Date32 => ColumnType::Date,
        DataType::Timestamp(_, _) => ColumnType::DateTime,
        other => {
            return Err(Error::UnsupportedArrowType {
                data_type: other.to_string(),
            });
        }
    })
}
