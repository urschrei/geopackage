//! Read and write layers through the Arrow C Data Interface.
//!
//! Feature data crosses the boundary as Arrow record batches in both
//! directions. [`gpkg_layer_read_arrow`], [`gpkg_layer_read_arrow_in`] and
//! [`gpkg_layer_read_arrow_filtered`] fill in an `ArrowArrayStream` that you
//! own and pull from. [`gpkg_layer_write_arrow`] consumes one. The geometry is
//! a GeoArrow WKB column whose Arrow metadata records the coordinate reference
//! system. That lets [`gpkg_create_layer_from_arrow_schema`] build a
//! destination layer from a source stream's schema, so the C API needs no
//! schema-description functions at all.
//!
//! The header declares the `ArrowSchema`, `ArrowArray` and `ArrowArrayStream`
//! structs verbatim from the Arrow specification, behind the guards the
//! specification names. A program that already has them from another
//! Arrow-exporting library keeps its own definitions.
//!
//! # Reading a layer
//!
//! Consume a stream as the specification says: call `get_schema` once, then
//! `get_next` until it returns a released array, then `release`.
//!
//! ```c
//! struct ArrowArrayStream stream;
//! memset(&stream, 0, sizeof(stream));
//! if (gpkg_layer_read_arrow(layer, &stream, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_layer_read_arrow", &error);
//! }
//!
//! struct ArrowSchema schema;
//! memset(&schema, 0, sizeof(schema));
//! if (stream.get_schema(&stream, &schema) != 0) {
//!     fprintf(stderr, "get_schema: %s\n", stream.get_last_error(&stream));
//!     stream.release(&stream);
//!     return 1;
//! }
//! printf("%lld columns\n", (long long)schema.n_children);
//! schema.release(&schema);
//!
//! long long rows = 0;
//! for (;;) {
//!     struct ArrowArray array;
//!     memset(&array, 0, sizeof(array));
//!     if (stream.get_next(&stream, &array) != 0) {
//!         fprintf(stderr, "get_next: %s\n", stream.get_last_error(&stream));
//!         stream.release(&stream);
//!         return 1;
//!     }
//!     if (array.release == NULL) {
//!         break;   // a released array is how the interface spells end of stream
//!     }
//!     rows += array.length;
//!     array.release(&array);
//! }
//! stream.release(&stream);
//! ```
//!
//! A stream reports failure the way the interface specifies, as a non-zero
//! return with the detail available from `get_last_error`, not through a
//! `gpkg_error_t`. The `gpkg_error_t` these calls take covers only opening the
//! stream.
//!
//! [`gpkg_layer_read_arrow_in`] also takes a bounding box, and returns the rows
//! whose geometry envelope intersects it:
//!
//! ```c
//! struct ArrowArrayStream stream;
//! memset(&stream, 0, sizeof(stream));
//! if (gpkg_layer_read_arrow_in(layer, -7.0, 53.0, -6.0, 54.0, &stream, &error)
//!     != GPKG_STATUS_OK) {
//!     return fail("gpkg_layer_read_arrow_in", &error);
//! }
//! // Pulled exactly as above.
//! ```
//!
//! # Writing a layer
//!
//! Create a layer from a schema, then fill it from a stream. Filling only
//! appends: nothing in this module updates or deletes an existing row. The
//! schema can come from any producer. If you take it from a source layer's own
//! stream, you can copy the layer without describing any columns:
//!
//! ```c
//! // The whole copy under one commit, so the SRS row, the layer and its rows
//! // land together or not at all.
//! gpkg_begin(dst, &error);
//! gpkg_add_epsg_srs(dst, 4326, &error);
//! gpkg_create_layer_from_arrow_schema(dst, "cities", &schema, true, &error);
//! schema.release(&schema);   // still the caller's: the call borrowed it
//!
//! gpkg_layer_t *dst_layer = gpkg_layer_open(dst, "cities", &error);
//!
//! struct ArrowArrayStream data;
//! memset(&data, 0, sizeof(data));
//! gpkg_layer_read_arrow(src_layer, &data, &error);
//!
//! uint64_t written = 0;
//! // Takes ownership of the stream: it is released by the call, whether the
//! // write succeeds or fails, and must not be released here.
//! if (gpkg_layer_write_arrow(dst_layer, &data, 1000, &written, &error)
//!     != GPKG_STATUS_OK) {
//!     gpkg_rollback(dst, NULL);
//!     return fail("gpkg_layer_write_arrow", &error);
//! }
//! gpkg_commit(dst, &error);
//! ```
//!
//! `examples/roundtrip.c` is the full program, including error handling, and
//! the test suite compiles and runs it.
//!
//! # Why the stream is implemented by hand
//!
//! `FFI_ArrowArrayStream::new` takes `Box<dyn RecordBatchReader + Send>`, so
//! the reader must be `Send + 'static`. `geopackage::ArrowBatches<'a>` is
//! neither. It borrows the layer, and its sequential variant keeps a
//! `&Connection`, which is not `Send` because `rusqlite::Connection` is not
//! `Sync`.
//!
//! The lifetime is the easy part. It is erased exactly as [`crate::handle`]
//! erases a layer's, and the same rule makes that sound: a stream counts as a
//! child of its container, so the container cannot close while the stream is
//! alive.
//!
//! `Send` cannot be supplied truthfully. Adding `unsafe impl Send` would assert
//! that a caller may pull batches from a thread other than the one holding the
//! connection, which is exactly what `Connection: !Sync` denies, and no
//! refcount can enforce the difference. Instead of making that claim, this
//! module fills in the four `ArrowArrayStream` callbacks itself. The struct's
//! fields are public, so nothing here relies on arrow-rs internals. What the
//! module avoids is arrow-rs's `Send` bound, which exists for arrow-rs's own
//! constructor rather than for the C interface: the C Data Interface makes no
//! threading claim of its own.
//!
//! So the stream follows this crate's ordinary rule: use it from one thread at
//! a time, together with its container and the container's other handles.
//!
//! # Panics across the boundary
//!
//! A panic inside one of the stream callbacks aborts the process rather than
//! unwinding into the caller's frames.

use std::ffi::{CString, c_char, c_int, c_void};

use arrow_array::Array;
use arrow_array::RecordBatchReader;
use arrow_array::array::StructArray;
use arrow_array::ffi::FFI_ArrowArray;
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow_schema::Schema;
use arrow_schema::ffi::FFI_ArrowSchema;
use geopackage::arrow::{ArrowBatches, ArrowReadOptions};
use geopackage::{BoundingBox, TableSchemaBuilder, ValueRef};

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::{ChildToken, Container, LayerHandle};
use crate::util::borrow_str;
use crate::value::{borrow_values, gpkg_value_t};

/// What a stream owns for as long as C holds it.
///
/// `batches` has had its borrow of the container erased. `_token` makes that
/// sound: it counts against the container's child count until the stream is
/// released.
struct StreamState {
    batches: ArrowBatches<'static>,
    last_error: Option<CString>,
    _token: ChildToken,
}

/// Drops the stream's state and blanks the stream, as the C Data Interface
/// requires of a release callback.
///
/// # Safety
///
/// `stream` must be a stream this module produced, not already released.
unsafe extern "C" fn release(stream: *mut FFI_ArrowArrayStream) {
    if stream.is_null() {
        return;
    }
    // SAFETY: the caller guarantees a live stream this module produced.
    let stream = unsafe { &mut *stream };
    stream.get_schema = None;
    stream.get_next = None;
    stream.get_last_error = None;
    if !stream.private_data.is_null() {
        // SAFETY: `private_data` was produced by `Box::into_raw` in
        // `export_batches` and has not been freed; releasing a stream twice is
        // prevented by clearing `release` below, which is what the C Data
        // Interface tells consumers to check.
        drop(unsafe { Box::from_raw(stream.private_data.cast::<StreamState>()) });
        stream.private_data = std::ptr::null_mut();
    }
    // Cleared last: a consumer treats a NULL release as "already released".
    stream.release = None;
}

/// Returns the stream's private state, or `None` when it has been released.
///
/// # Safety
///
/// `stream` must be a stream this module produced.
unsafe fn state<'a>(stream: *mut FFI_ArrowArrayStream) -> Option<&'a mut StreamState> {
    if stream.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live stream this module produced.
    let private = unsafe { (*stream).private_data };
    if private.is_null() {
        return None;
    }
    // SAFETY: `private_data` is a `StreamState` this module boxed, and the
    // stream is used from one thread at a time by the crate's threading rule.
    Some(unsafe { &mut *private.cast::<StreamState>() })
}

/// Writes the stream's schema into `out`.
///
/// # Safety
///
/// `stream` must be a stream this module produced and `out` must be writable.
unsafe extern "C" fn get_schema(
    stream: *mut FFI_ArrowArrayStream,
    out: *mut FFI_ArrowSchema,
) -> c_int {
    // SAFETY: forwarded to this callback's contract.
    let Some(state) = (unsafe { state(stream) }) else {
        return EINVAL;
    };
    match FFI_ArrowSchema::try_from(state.batches.schema().as_ref()) {
        Ok(schema) => {
            // SAFETY: the caller guarantees `out` is writable and uninitialised
            // from the consumer's point of view, which is what the C Data
            // Interface specifies for an out-parameter.
            unsafe { std::ptr::write_unaligned(out, schema) };
            0
        }
        Err(error) => {
            state.last_error = CString::new(error.to_string()).ok();
            EIO
        }
    }
}

/// Writes the next batch into `out`, or a released array at end of stream.
///
/// # Safety
///
/// As [`get_schema`].
unsafe extern "C" fn get_next(
    stream: *mut FFI_ArrowArrayStream,
    out: *mut FFI_ArrowArray,
) -> c_int {
    // SAFETY: forwarded to this callback's contract.
    let Some(state) = (unsafe { state(stream) }) else {
        return EINVAL;
    };
    match state.batches.next() {
        // A released array is how the interface spells end of stream.
        None => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { std::ptr::write_unaligned(out, FFI_ArrowArray::empty()) };
            0
        }
        Some(Ok(batch)) => {
            let array = FFI_ArrowArray::new(&StructArray::from(batch).to_data());
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { std::ptr::write_unaligned(out, array) };
            0
        }
        Some(Err(error)) => {
            state.last_error = CString::new(error.to_string()).ok();
            EIO
        }
    }
}

/// Returns the message from the last failing callback, or NULL.
///
/// The consumer borrows the message. It stays valid until the next call on the
/// stream.
///
/// # Safety
///
/// As [`get_schema`].
unsafe extern "C" fn get_last_error(stream: *mut FFI_ArrowArrayStream) -> *const c_char {
    // SAFETY: forwarded to this callback's contract.
    let Some(state) = (unsafe { state(stream) }) else {
        return std::ptr::null();
    };
    match &state.last_error {
        Some(message) => message.as_ptr(),
        None => std::ptr::null(),
    }
}

/// The errno values `EINVAL` and `EIO`, which the C Data Interface suggests
/// for a misused and a failing stream. They are defined here rather than taken
/// from libc, which this crate does not otherwise depend on.
const EINVAL: c_int = 22;
const EIO: c_int = 5;

/// Builds a stream over `batches`, holding `token` to keep the container
/// alive.
fn export_batches(batches: ArrowBatches<'static>, token: ChildToken) -> FFI_ArrowArrayStream {
    let state = Box::new(StreamState {
        batches,
        last_error: None,
        _token: token,
    });
    FFI_ArrowArrayStream {
        get_schema: Some(get_schema),
        get_next: Some(get_next),
        get_last_error: Some(get_last_error),
        release: Some(release),
        private_data: Box::into_raw(state).cast::<c_void>(),
    }
}

/// Reads a layer as an Arrow C Data Interface stream.
///
/// Fills in `out` with a stream that you own and must release through its own
/// `release` callback, as the C Data Interface specifies. The stream borrows
/// the layer's container, so the container cannot be closed until the stream
/// has been released, as with a layer handle.
///
/// ```c
/// struct ArrowArrayStream stream;
/// memset(&stream, 0, sizeof(stream));
/// if (gpkg_layer_read_arrow(layer, &stream, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_layer_read_arrow", &error);
/// }
///
/// for (;;) {
///     struct ArrowArray array;
///     memset(&array, 0, sizeof(array));
///     if (stream.get_next(&stream, &array) != 0) {
///         fprintf(stderr, "get_next: %s\n", stream.get_last_error(&stream));
///         break;
///     }
///     if (array.release == NULL) {
///         break;   // end of stream
///     }
///     // ... consume array.length rows ...
///     array.release(&array);
/// }
/// stream.release(&stream);
/// ```
///
/// Rows arrive in primary-key order, in batches of up to 65,536 rows. A batch
/// ends early if its geometry would otherwise exceed a byte ceiling, so a layer
/// of very large geometries still reads. The ceiling is the smaller of 2 GB
/// (the reach of the geometry column's 32-bit Arrow offsets) and a quarter of
/// the machine's memory.
///
/// If reading fails part-way through, `get_next` returns non-zero and
/// `get_last_error` has the detail, as the interface specifies. The `error`
/// out-parameter covers only opening the stream.
///
/// Batches come back on one thread. The threaded reader assigns key windows to
/// workers, which needs a second connection per worker. A stream handed to C
/// is pulled on the caller's thread instead.
///
/// # Safety
///
/// `layer` must be a live layer handle, `out` must point at writable storage
/// for an `ArrowArrayStream`, and `error` must be NULL or writable. The
/// returned stream follows the threading rule for its container: use it, and
/// release it, only while no other thread is using that container or any
/// handle taken from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_read_arrow(
    layer: *const LayerHandle,
    out: *mut FFI_ArrowArrayStream,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    unsafe { read_arrow_inner(layer, None, None, out, error) }
}

/// Reads the rows of a layer that intersect a bounding box, as an Arrow stream.
///
/// The box is in the layer's own spatial reference system. A row is returned
/// when its geometry's envelope intersects the box, edges included. An
/// envelope is not the geometry, so the result is a superset of the rows whose
/// geometries actually intersect. To test the geometries themselves, use the
/// WKB the stream yields.
///
/// The read uses the layer's RTree index when it has a usable one, and
/// otherwise a full scan with the same filter. Either way it returns the same
/// rows. `gpkg_layer_has_spatial_index` tells you which it will use.
///
/// ```c
/// struct ArrowArrayStream stream;
/// memset(&stream, 0, sizeof(stream));
/// if (gpkg_layer_read_arrow_in(layer, -7.0, 53.0, -6.0, 54.0, &stream, &error)
///     != GPKG_STATUS_OK) {
///     return fail("gpkg_layer_read_arrow_in", &error);
/// }
/// // Pulled and released exactly as gpkg_layer_read_arrow's stream is.
/// ```
///
/// Fails with `GPKG_STATUS_NOT_FOUND` on a layer with no geometry column,
/// which has nothing to intersect.
///
/// # Safety
///
/// As [`gpkg_layer_read_arrow`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_read_arrow_in(
    layer: *const LayerHandle,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    out: *mut FFI_ArrowArrayStream,
    error: *mut gpkg_error_t,
) -> Status {
    let bbox = BoundingBox::new(min_x, min_y, max_x, max_y);
    // SAFETY: forwarded to this function's contract.
    unsafe { read_arrow_inner(layer, Some(bbox), None, out, error) }
}

/// Reads the rows matching a SQL `WHERE` clause, a bounding box, or both, as an
/// Arrow stream.
///
/// This is the general form of the three readers. `bbox` is NULL or four
/// doubles (`min_x`, `min_y`, `max_x`, `max_y`), read as by
/// `gpkg_layer_read_arrow_in`. `where_clause` is NULL or a SQL clause, as
/// described below. With both NULL, this is `gpkg_layer_read_arrow`. With both
/// set, the stream yields the rows that satisfy both.
///
/// The clause is **raw SQL, trusted from the caller**. It is appended to the
/// query in parentheses, and is not parsed or sanitised, just as the Rust
/// crate's `Layer::select` treats it. Its placeholders are `?1` to `?N`, and
/// `params` bind them in array order. The SQL the read adds around the clause
/// never collides with that numbering. `params` reuses the writer's
/// `gpkg_value_t`, and is borrowed only for the duration of this call.
///
/// To read one row by feature id, use the clause `fid = ?1` with the id as its
/// parameter:
///
/// ```c
/// gpkg_value_t fid = {GPKG_VALUE_KIND_INTEGER, {.integer = 17}};
/// struct ArrowArrayStream stream;
/// memset(&stream, 0, sizeof(stream));
/// if (gpkg_layer_read_arrow_filtered(layer, NULL, "fid = ?1", &fid, 1,
///                                    &stream, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_layer_read_arrow_filtered", &error);
/// }
/// // Pulled and released exactly as gpkg_layer_read_arrow's stream is.
/// ```
///
/// Each batch is its own query, so a clause SQLite cannot prepare is reported
/// the interface's way, through the stream's first `get_next` and
/// `get_last_error`. The `error` out-parameter covers only opening the stream.
/// Passing `params` without a clause for them fails with
/// `GPKG_STATUS_INVALID_ARGUMENT`. A bounding box on a layer with no geometry
/// column fails with `GPKG_STATUS_NOT_FOUND`, as for
/// `gpkg_layer_read_arrow_in`.
///
/// # Safety
///
/// As [`gpkg_layer_read_arrow`], and additionally: `bbox` must be NULL or
/// point at four readable doubles; `where_clause` must be NULL or a
/// NUL-terminated UTF-8 string; `params` must be NULL (only when `params_len`
/// is 0) or point at `params_len` readable `gpkg_value_t` values, each with
/// the payload its kind promises, borrowed only for the duration of this
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_read_arrow_filtered(
    layer: *const LayerHandle,
    bbox: *const f64,
    where_clause: *const c_char,
    params: *const gpkg_value_t,
    params_len: usize,
    out: *mut FFI_ArrowArrayStream,
    error: *mut gpkg_error_t,
) -> Status {
    let bbox = if bbox.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees four readable doubles when non-NULL.
        // Read unaligned because C makes no alignment promise about an array
        // on its own stack.
        let [min_x, min_y, max_x, max_y] =
            unsafe { std::ptr::read_unaligned(bbox.cast::<[f64; 4]>()) };
        Some(BoundingBox::new(min_x, min_y, max_x, max_y))
    };
    let clause = if where_clause.is_null() {
        if params_len != 0 {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe {
                set_error(
                    error,
                    Status::InvalidArgument,
                    "params were passed without a where clause to bind them",
                )
            };
            return Status::InvalidArgument;
        }
        None
    } else {
        // SAFETY: forwarded to the caller's guarantee on `where_clause`.
        let Some(text) = (unsafe { borrow_str(where_clause, error, "where clause") }) else {
            return Status::BadArgument;
        };
        Some(text)
    };
    let values = match clause {
        // SAFETY: forwarded to the caller's guarantee on `params`.
        Some(_) => match unsafe { borrow_values(params, params_len, error) } {
            Some(values) => values,
            None => return Status::BadArgument,
        },
        None => Vec::new(),
    };
    // SAFETY: forwarded to this function's contract.
    unsafe { read_arrow_inner(layer, bbox, clause.map(|text| (text, values)), out, error) }
}

/// Implements the three readers.
///
/// # Safety
///
/// As [`gpkg_layer_read_arrow`].
unsafe fn read_arrow_inner(
    layer: *const LayerHandle,
    bbox: Option<BoundingBox>,
    sql_filter: Option<(&str, Vec<ValueRef<'_>>)>,
    out: *mut FFI_ArrowArrayStream,
    error: *mut gpkg_error_t,
) -> Status {
    if layer.is_null() || out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "layer handle or out is NULL") };
        return Status::BadArgument;
    }

    // SAFETY: the caller guarantees a live layer handle.
    let handle = unsafe { &*layer };
    // A single thread: the stream is pulled by C on the caller's thread, and
    // the threaded reader would need a connection per worker.
    let options = ArrowReadOptions::default().with_threads(1);
    let opened = match (bbox, &sql_filter) {
        (Some(bbox), Some((clause, params))) => handle
            .layer()
            .read_arrow_in_where(bbox, clause, params, options),
        (Some(bbox), None) => handle.layer().read_arrow_in(bbox, options),
        (None, Some((clause, params))) => handle.layer().read_arrow_where(clause, params, options),
        (None, None) => handle.layer().read_arrow(options),
    };
    let batches = match opened {
        Ok(batches) => batches,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };

    // SAFETY: `batches` borrows the layer, which borrows the container, and the
    // token below keeps the container from closing until the stream is
    // released. So the erased-to-`'static` borrow never outlives what it points
    // at, by the same argument as `handle::Container::adopt`. The transmute
    // changes only the lifetime parameter.
    let erased: ArrowBatches<'static> = unsafe { std::mem::transmute(batches) };
    let token = handle.token();

    // SAFETY: the caller guarantees `out` points at writable storage for an
    // `ArrowArrayStream`. Written unaligned because C makes no alignment
    // promise about a struct it declared on its own stack.
    unsafe { std::ptr::write_unaligned(out, export_batches(erased, token)) };
    Status::Ok
}

/// Writes an Arrow C Data Interface stream into a layer.
///
/// Writing only appends. Every batch adds rows, and no existing row is changed
/// or removed. That suits a bulk load. It also means that re-writing rows that
/// supply their own feature ids fails on the primary key rather than replacing
/// them. To change or remove an existing feature, use a `gpkg_writer_t` from
/// `gpkg_layer_writer`.
///
/// Every column in the stream's schema must exist in the layer. A column the
/// layer does not have is rejected rather than dropped, because discarding
/// data you asked to write would be worse than rejecting it. A layer column
/// that the stream does not name is left to its default. A type the layer
/// cannot store fails with `GPKG_STATUS_INVALID_ARGUMENT`. If you build the
/// layer with `gpkg_create_layer_from_arrow_schema` from the same schema, the
/// two agree by construction.
///
/// A stream column matching the layer's primary key supplies each row's fid,
/// and a row with NULL there is assigned one. A fid the table already contains
/// fails the write, so when you append to a layer that already has rows, omit
/// that column or supply NULLs in it.
///
/// Takes ownership of `stream`, as the C Data Interface specifies for a moved
/// stream. The call releases it whether the write succeeds or fails, and you
/// must not release it again. If `out_rows` is non-NULL, it receives the
/// number of rows written.
///
/// ```c
/// struct ArrowArrayStream data;
/// memset(&data, 0, sizeof(data));
/// if (gpkg_layer_read_arrow(src_layer, &data, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_layer_read_arrow", &error);
/// }
///
/// uint64_t written = 0;
/// // `data` is consumed here, and must not be released afterwards.
/// if (gpkg_layer_write_arrow(dst_layer, &data, 1000, &written, &error)
///     != GPKG_STATUS_OK) {
///     return fail("gpkg_layer_write_arrow", &error);
/// }
/// printf("wrote %llu rows\n", (unsigned long long)written);
/// ```
///
/// `batch_size` is the number of rows per write transaction, and `0` writes
/// everything in one transaction. Inside a transaction you opened with
/// `gpkg_begin`, every batch belongs to that transaction. The batch size then
/// sets only how many rows are held at once, not what becomes durable.
///
/// If the write fails part-way through, the batches that have already
/// committed stay. So `batch_size` also decides how much a failed write can
/// leave behind. With `0` it leaves nothing, because there is one transaction.
/// Inside your own transaction, nothing is durable until you commit, and
/// `gpkg_rollback` discards all of it.
///
/// If the layer has a spatial index, the write brings it up to date.
///
/// # Safety
///
/// `layer` must be a live layer handle, `stream` must point at a stream the
/// caller owns and has not released, `out_rows` must be NULL or writable, and
/// `error` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_write_arrow(
    layer: *const LayerHandle,
    stream: *mut FFI_ArrowArrayStream,
    batch_size: usize,
    out_rows: *mut u64,
    error: *mut gpkg_error_t,
) -> Status {
    if layer.is_null() || stream.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "layer handle or stream is NULL") };
        return Status::BadArgument;
    }

    // SAFETY: the caller guarantees a stream they own and have not released.
    // `from_raw` performs the interface's move: the caller's struct is left
    // released, so releasing it again is a no-op rather than a double free.
    let reader = match unsafe { ArrowArrayStreamReader::from_raw(stream) } {
        Ok(reader) => reader,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_error(error, Status::InvalidArgument, &err.to_string()) };
            return Status::InvalidArgument;
        }
    };

    // SAFETY: the caller guarantees a live layer handle.
    let handle = unsafe { &*layer };
    // The reader is an `Iterator<Item = Result<RecordBatch, ArrowError>>`,
    // which is exactly what the write path takes. No lifetime erasure and no
    // `Send` claim are needed here: an imported stream owns everything it
    // reads from, which is what makes this direction the simple one.
    match handle.layer().write_arrow(reader, batch_size) {
        Ok(fids) => {
            if !out_rows.is_null() {
                // SAFETY: the caller guarantees `out_rows` is writable.
                unsafe { *out_rows = fids.len() as u64 };
            }
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Creates a layer whose columns come from an Arrow schema.
///
/// The call borrows the schema rather than consuming it, so you still own it
/// and must release it. To copy a layer without describing any columns
/// yourself, take the schema from the source stream's `get_schema` and pass it
/// straight here.
///
/// ```c
/// struct ArrowArrayStream src_stream;
/// memset(&src_stream, 0, sizeof(src_stream));
/// gpkg_layer_read_arrow(src_layer, &src_stream, &error);
///
/// struct ArrowSchema schema;
/// memset(&schema, 0, sizeof(schema));
/// src_stream.get_schema(&src_stream, &schema);
/// src_stream.release(&src_stream);   // the schema outlives the stream
///
/// gpkg_add_epsg_srs(dst, 4326, &error);
/// if (gpkg_create_layer_from_arrow_schema(dst, "cities", &schema, true, &error)
///     != GPKG_STATUS_OK) {
///     schema.release(&schema);
///     return fail("gpkg_create_layer_from_arrow_schema", &error);
/// }
/// schema.release(&schema);
/// ```
///
/// The geometry column and its spatial reference system come from the
/// schema's GeoArrow metadata. The referenced SRS must already exist in the
/// destination, and `gpkg_add_epsg_srs` can add it. The geometry column is
/// declared `GEOMETRY` rather than with the source's own type, because the
/// GeoArrow WKB encoding does not record that type.
///
/// `spatial_index` requests an RTree index over that column, the usual choice
/// for a layer you will query by bounding box. `gpkg_layer_create_spatial_index`
/// can add one later, at the cost of a pass over the rows.
///
/// Fails with `GPKG_STATUS_ALREADY_EXISTS` when the file already has a table
/// of that name, and with `GPKG_STATUS_NOT_FOUND` when the schema names a
/// spatial reference system the file does not register.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `name` a NUL-terminated UTF-8
/// string, `schema` a valid `ArrowSchema` the caller owns, and `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_create_layer_from_arrow_schema(
    gpkg: *const Container,
    name: *const c_char,
    schema: *const FFI_ArrowSchema,
    spatial_index: bool,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() || schema.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle or schema is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to the caller's guarantee on `name`.
    let Some(name) = (unsafe { borrow_str(name, error, "layer name") }) else {
        return Status::BadArgument;
    };

    // SAFETY: the caller guarantees a valid `ArrowSchema` they still own.
    // Borrowed rather than moved: `TryFrom<&FFI_ArrowSchema>` copies what it
    // needs, so the caller's schema is untouched and still theirs to release.
    let imported = match Schema::try_from(unsafe { &*schema }) {
        Ok(imported) => imported,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_error(error, Status::InvalidArgument, &err.to_string()) };
            return Status::InvalidArgument;
        }
    };

    // SAFETY: the caller guarantees a live container handle.
    let container = unsafe { &*gpkg };
    let builder = match TableSchemaBuilder::new(name).from_arrow_schema(&imported) {
        Ok(builder) => builder.spatial_index(spatial_index),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };

    match container.gpkg().create_layer(&builder) {
        Ok(_) => Status::Ok,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Registers an EPSG spatial reference system in the file.
///
/// A layer cannot name an SRS the file does not register, so call this before
/// creating a layer that uses one.
///
/// # Safety
///
/// `gpkg` must be a live container handle and `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_add_epsg_srs(
    gpkg: *const Container,
    code: i32,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live container handle.
    match unsafe { (*gpkg).gpkg().add_epsg_srs(code) } {
        Ok(_) => Status::Ok,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}
