use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, DataType, Schema, TimeUnit};
use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::types::{ColumnType, GeometryType};

use crate::{Error, Layer, Result};

use super::schema::{
    EXTENSION_METADATA_KEY, EXTENSION_NAME_KEY, GEOARROW_WKB, data_type_for, epsg_code,
};

/// Where each of a layer's columns sits in a batch, shared by all its rows.
struct RowLayout {
    fid: Option<usize>,
    geometry: Option<usize>,
    /// Batch column index for each of the layer's value columns, in order.
    /// `None` for a column the batch does not include, which is written as
    /// NULL.
    values: Vec<Option<usize>>,
}

/// One row of a [`RecordBatch`], as a view instead of a copy.
///
/// Holding the batch and an index, instead of owned values, lets the write
/// path bind strings and blobs directly from the Arrow arrays. Both handles are
/// `Arc`, so a row costs two reference-count increments. Owned values would
/// cost an allocation per text cell plus a copy of the geometry.
struct ArrowRow {
    batch: Arc<RecordBatch>,
    layout: Arc<RowLayout>,
    row: usize,
}

/// A row, or the error that prevented it being built.
///
/// Batches are split into rows lazily, so an error has to travel with the rows
/// instead of leaving the iterator by a separate route. Writing an error value
/// fails the write, which rolls back the transaction like any other write
/// error.
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
    #[hotpath::measure(label = "arrow::ArrowRow::write")]
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
    /// This is the columnar equivalent of [`crate::Layer::write_all`], and uses
    /// the same code. Batching, the bulk spatial-index decision and the single
    /// transaction all behave identically. The difference is that a geometry
    /// arrives as WKB and stays WKB. It gains a GPB header instead of being
    /// parsed into a geometry object and serialised again.
    ///
    /// The reader's schema must name columns that this layer has. Extra columns
    /// in the batch are an error, not ignored, because silently dropping data
    /// you asked to write is worse than rejecting it. A layer column that the
    /// batch doesn't name is left to its default.
    ///
    /// Each batch column must fit its layer column: the Arrow type a read
    /// gives that column (see the [module documentation](super)), a narrower
    /// or unsigned integer, `Float32`, `LargeUtf8` or `LargeBinary`, or a
    /// timestamp in any unit. This is checked once per batch, before any of
    /// its rows is written.
    ///
    /// Values are stored as the row path stores them: a timestamp as `DATETIME`
    /// text in the strict 1.4 form, normalised to UTC and floored to the
    /// millisecond; a NaN as NULL and a negative zero as zero, as SQLite
    /// stores them.
    ///
    /// A [`RecordBatchReader`](arrow_array::RecordBatchReader) doesn't say how
    /// many rows it will produce. The bulk index path handles this by buffering
    /// instead of trusting a size hint (issue #17).
    ///
    /// Returns the assigned feature ids, in the order the rows were written.
    ///
    /// # Errors
    ///
    /// [`Error::ArrowValueMismatch`] for a column whose Arrow type does not fit
    /// the layer's declared type, or a `UInt64` value above `i64::MAX`;
    /// [`Error::InvalidDateTimeValue`] for a date or timestamp outside years 0
    /// to 9999; [`Error`] for the write itself; and any error the reader
    /// yields.
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
        // Each value column with the Arrow type a read gives it, which decides
        // the Arrow types a write accepts for it.
        let value_columns: Vec<(String, DataType)> = self
            .value_columns()
            .iter()
            .map(|column| (column.name.clone(), data_type_for(column)))
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

/// Works out where each of the layer's columns sits in this batch.
///
/// The layout is computed once per batch and shared, so a row stores two `Arc`
/// handles instead of copying anything.
#[hotpath::measure(label = "arrow::layout_of")]
fn layout_of(
    batch: &RecordBatch,
    primary_key: Option<&str>,
    geometry: Option<&str>,
    value_columns: &[(String, DataType)],
) -> Result<RowLayout> {
    let schema = batch.schema();
    for field in schema.fields() {
        if Some(field.name().as_str()) == primary_key || Some(field.name().as_str()) == geometry {
            continue;
        }
        let Some((_, read_type)) = value_columns.iter().find(|(name, _)| name == field.name())
        else {
            return Err(Error::NoSuchColumn {
                table_name: String::new(),
                column_name: field.name().clone(),
            });
        };
        if !writes_into(field.data_type(), read_type) {
            return Err(Error::ArrowValueMismatch {
                column: field.name().clone(),
                expected: type_name(read_type),
                found: "an Arrow array of a type the column does not take",
            });
        }
    }

    let index_of = |name: &str| schema.fields().iter().position(|f| f.name() == name);
    Ok(RowLayout {
        fid: primary_key.and_then(index_of),
        geometry: geometry.and_then(index_of),
        values: value_columns
            .iter()
            .map(|(name, _)| index_of(name))
            .collect(),
    })
}

/// Returns `true` if a batch column of `data_type` may be written into a
/// layer column that a read gives `read_type`: the same type, or a narrower
/// or unsigned integer, a narrower float, the large forms of text and binary,
/// and a timestamp in any unit or zone.
fn writes_into(data_type: &DataType, read_type: &DataType) -> bool {
    use DataType as T;
    match read_type {
        T::Int64 => matches!(
            data_type,
            T::Int8 | T::Int16 | T::Int32 | T::Int64 | T::UInt8 | T::UInt16 | T::UInt32 | T::UInt64
        ),
        T::Float64 => matches!(data_type, T::Float32 | T::Float64),
        T::Utf8 => matches!(data_type, T::Utf8 | T::LargeUtf8),
        T::Binary => matches!(data_type, T::Binary | T::LargeBinary),
        T::Timestamp(..) => matches!(data_type, T::Timestamp(..)),
        other => data_type == other,
    }
}

/// The name of the Arrow type a read gives a column, for an error.
fn type_name(read_type: &DataType) -> &'static str {
    match read_type {
        DataType::Boolean => "Boolean",
        DataType::Int64 => "Int64 or a narrower integer",
        DataType::Float64 => "Float64 or Float32",
        DataType::Utf8 => "Utf8 or LargeUtf8",
        DataType::Binary => "Binary or LargeBinary",
        DataType::Date32 => "Date32",
        DataType::Timestamp(..) => "Timestamp",
        _ => "the column's Arrow type",
    }
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

/// Reads one `Int64` cell, for the feature id.
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

/// Binds one attribute cell, borrowing from the array wherever possible.
///
/// This is the inverse of the read path's mapping, and deliberately narrower.
/// It accepts what [`Layer::arrow_schema`] produces, so a round trip works,
/// plus the narrower integer and float widths that other producers are likely
/// to emit.
///
/// Strings and blobs are bound as slices into the Arrow buffers, which is the
/// purpose of this function. They are already contiguous there, so copying
/// them into a `Value` first would allocate once per cell for no benefit.
/// `DATE` and `DATETIME` are the exception, because a GeoPackage stores them
/// as text, which has to be produced.
fn bind_value<'a>(
    column: &'a ArrayRef,
    row: usize,
    position: usize,
    batch: &RecordBatch,
) -> Result<rusqlite::types::ToSqlOutput<'a>> {
    use arrow_array::types::{
        Date32Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
        TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
        TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
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
        DataType::UInt8 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<UInt8Type>()
                .ok_or_else(|| mismatch("UInt8"))?
                .value(row),
        ))),
        DataType::UInt16 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<UInt16Type>()
                .ok_or_else(|| mismatch("UInt16"))?
                .value(row),
        ))),
        DataType::UInt32 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<UInt32Type>()
                .ok_or_else(|| mismatch("UInt32"))?
                .value(row),
        ))),
        // SQLite's INTEGER is signed, so a value above `i64::MAX` has no
        // storage.
        DataType::UInt64 => owned(SqlV::Integer(
            i64::try_from(
                column
                    .as_primitive_opt::<UInt64Type>()
                    .ok_or_else(|| mismatch("UInt64"))?
                    .value(row),
            )
            .map_err(|_too_large| Error::ArrowValueMismatch {
                column: name(),
                expected: "an integer no larger than i64::MAX",
                found: "a larger UInt64 value",
            })?,
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
        // Stored in the strict 1.4 form, UTC and floored to the millisecond,
        // whatever the unit: a finer timestamp loses its sub-millisecond
        // digits, as the column's text form has none.
        DataType::Timestamp(unit, _) => {
            let micros = match unit {
                TimeUnit::Second => column
                    .as_primitive_opt::<TimestampSecondType>()
                    .ok_or_else(|| mismatch("Timestamp"))?
                    .value(row)
                    .checked_mul(1_000_000),
                TimeUnit::Millisecond => column
                    .as_primitive_opt::<TimestampMillisecondType>()
                    .ok_or_else(|| mismatch("Timestamp"))?
                    .value(row)
                    .checked_mul(1_000),
                TimeUnit::Microsecond => Some(
                    column
                        .as_primitive_opt::<TimestampMicrosecondType>()
                        .ok_or_else(|| mismatch("Timestamp"))?
                        .value(row),
                ),
                TimeUnit::Nanosecond => Some(
                    column
                        .as_primitive_opt::<TimestampNanosecondType>()
                        .ok_or_else(|| mismatch("Timestamp"))?
                        .value(row)
                        .div_euclid(1_000),
                ),
            };
            let text = micros
                .ok_or(geopackage_core::datetime::DateTimeError::OutOfRange(
                    "datetime",
                ))
                .and_then(DateTime::from_micros_since_epoch)
                .and_then(DateTime::format_strict)
                .map_err(out_of_range)?;
            owned(SqlV::Text(text))
        }
        other => Err(Error::UnsupportedArrowType {
            data_type: other.to_string(),
        }),
    }
}

impl crate::TableSchemaBuilder {
    /// Derives a layer definition from an Arrow schema.
    ///
    /// This is the inverse of [`Layer::arrow_schema`], for creating a layer to
    /// receive [`Layer::write_arrow`]. You can still override anything the
    /// builder normally takes afterwards.
    ///
    /// The mapping is the read mapping in reverse, with three points to note:
    ///
    /// - **Integer widths are kept here, but not when reading.** `Int8`
    ///   becomes `TINYINT`, `Int16` `SMALLINT`, `Int32` `MEDIUMINT`, and
    ///   `Int64` `INTEGER`. Reading collapses all four to `Int64`, because
    ///   SQLite doesn't enforce a declared width. A layer round-tripped through
    ///   Arrow therefore comes back with every integer column widened to
    ///   `INTEGER`. The data is unchanged, but the declared type is not.
    /// - **The geometry column is found by its `geoarrow.wkb` extension name**,
    ///   not by position or by name. Its declared type is `GEOMETRY`, which
    ///   accepts any geometry, because WKB doesn't say what it will contain.
    ///   The SRS comes from the field's CRS metadata if that is an
    ///   `EPSG:<code>` authority code, and is otherwise `0`, the spec's
    ///   undefined value.
    /// - **A field named as the primary key is skipped**, not made an attribute
    ///   column, because the builder creates the key itself. Call
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

/// Returns the GeoPackage column type for an Arrow type, the inverse of the
/// mapping in the [module documentation](super).
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
