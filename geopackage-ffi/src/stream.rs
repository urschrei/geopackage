//! The Arrow C Data Interface data plane.
//!
//! Feature data crosses the boundary as Arrow record batches, in both
//! directions. [`gpkg_layer_read_arrow`] and [`gpkg_layer_read_arrow_in`] fill
//! in an `ArrowArrayStream` the caller owns and pulls from;
//! [`gpkg_layer_write_arrow`] consumes one. The geometry is a GeoArrow WKB
//! column whose Arrow metadata carries the coordinate reference system, which
//! is what lets [`gpkg_create_layer_from_arrow_schema`] build a destination
//! layer out of a source stream's schema, with no schema-description API on
//! this side at all.
//!
//! The `ArrowSchema`, `ArrowArray` and `ArrowArrayStream` structs are declared
//! in the header, verbatim from the Arrow specification and behind the guards
//! it names, so a consumer that already has them from another Arrow-exporting
//! library keeps its own definitions.
//!
//! # Reading a layer
//!
//! A stream is consumed exactly as the specification says: `get_schema` once,
//! then `get_next` until it hands back a released array, then `release`.
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
//! A stream failure is reported the interface's way, as a non-zero return with
//! the detail behind `get_last_error`, rather than through a `gpkg_error_t`.
//! The `gpkg_error_t` these calls take covers only opening the stream.
//!
//! [`gpkg_layer_read_arrow_in`] is the same with a bounding box, and returns
//! the rows whose geometry envelope intersects it:
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
//! A layer is created from a schema and then filled from a stream, and filling
//! is appending: nothing on this side updates or deletes a row that is already
//! there. The schema
//! can come from anywhere that produces one; taking it from a source layer's
//! own stream is what makes a copy possible without describing any columns:
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
//! `examples/roundtrip.c` is that program in full, error handling included, and
//! is compiled and run by the test suite.
//!
//! # Why this is hand-rolled
//!
//! `FFI_ArrowArrayStream::new` takes `Box<dyn RecordBatchReader + Send>`, so
//! `Send + 'static`. `geopackage::ArrowBatches<'a>` is neither: it borrows the
//! layer, and its sequential variant holds a `&Connection`, which is not `Send`
//! because `rusqlite::Connection` is not `Sync`.
//!
//! The lifetime is the easy half, erased exactly as [`crate::handle`] erases a
//! layer's, and made sound by the same rule: a stream counts as a child of its
//! container, so the container refuses to close while the stream is alive.
//!
//! `Send` is the half that cannot be supplied truthfully. Adding
//! `unsafe impl Send` would assert that a caller may pull batches from a thread
//! other than the one holding the connection, which is exactly what
//! `Connection: !Sync` denies, and no refcount can enforce the difference.
//! Rather than claim it, this module fills in the four `ArrowArrayStream`
//! callbacks itself. The struct's fields are public, so nothing here relies on
//! arrow-rs internals; what it avoids is arrow-rs's `Send` bound, which exists
//! for its own constructor rather than for the C interface, since the C Data
//! Interface makes no threading claim of its own.
//!
//! The stream therefore inherits this crate's ordinary rule: use it on the
//! thread that created it.
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
use geopackage::{BoundingBox, TableSchemaBuilder};

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::{ChildToken, Container, LayerHandle};
use crate::util::borrow_str;

/// What a stream owns for as long as C holds it.
///
/// `batches` has had its borrow of the container erased; `_token` is what makes
/// that sound, by counting against the container's child count until the stream
/// is released.
struct StreamState {
    batches: ArrowBatches<'static>,
    last_error: Option<CString>,
    _token: ChildToken,
}

/// Release callback: drop the state, and blank the stream as the C Data
/// Interface requires.
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

/// The stream's private state, or `None` when it has been released.
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

/// Write the stream's schema into `out`.
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

/// Write the next batch into `out`, or a released array at end of stream.
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

/// The message from the last failing callback, or NULL.
///
/// The consumer borrows this; it stays valid until the next call on the stream.
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

/// `EINVAL` and `EIO`, the errno values the C Data Interface suggests for a
/// misused and a failing stream. Spelled out rather than pulled from libc,
/// which this crate does not otherwise depend on.
const EINVAL: c_int = 22;
const EIO: c_int = 5;

/// Build a stream over `batches`, taking `token` as the thing that keeps the
/// container alive.
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

/// Read a layer as an Arrow C Data Interface stream.
///
/// `out` is filled in with a stream the caller owns and must release through
/// its own `release` callback, as the C Data Interface specifies. The stream
/// borrows the layer's container: the container cannot be closed until the
/// stream has been released, exactly as for a layer handle.
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
/// whose geometry would cross a byte ceiling is emitted short of that, so a
/// layer of very large geometries still reads. The ceiling is the smaller of
/// 2 GB, which is as far as the geometry column's 32-bit Arrow offsets reach,
/// and a quarter of the machine's memory.
///
/// A failure part-way through is reported the interface's way: `get_next`
/// returns non-zero and `get_last_error` has the detail. The `error`
/// out-parameter here covers only opening the stream.
///
/// Batches come back on one thread. The threaded reader assigns key windows to
/// workers, which needs a second connection per worker; a stream handed to C is
/// pulled on the caller's thread instead.
///
/// # Safety
///
/// `layer` must be a live layer handle, `out` must point at writable storage
/// for an `ArrowArrayStream`, and `error` must be NULL or writable. The
/// returned stream must be used from the thread that created it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_read_arrow(
    layer: *const LayerHandle,
    out: *mut FFI_ArrowArrayStream,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    unsafe { read_arrow_inner(layer, None, out, error) }
}

/// Read the rows of a layer intersecting a bounding box, as an Arrow stream.
///
/// The box is in the layer's own spatial reference system, and a row is
/// returned when its geometry's envelope intersects the box, edges included.
/// An envelope is not the geometry, so the rows are a superset of those that
/// actually intersect: testing the geometries themselves is the caller's, on
/// the WKB the stream carries.
///
/// Served by the layer's RTree index when it has a usable one, and by a full
/// scan with the same filter otherwise, returning the same rows either way.
/// `gpkg_layer_has_spatial_index` says which it will be.
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
    unsafe { read_arrow_inner(layer, Some(bbox), out, error) }
}

/// The body of both readers.
///
/// # Safety
///
/// As [`gpkg_layer_read_arrow`].
unsafe fn read_arrow_inner(
    layer: *const LayerHandle,
    bbox: Option<BoundingBox>,
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
    let opened = match bbox {
        Some(bbox) => handle.layer().read_arrow_in(bbox, options),
        None => handle.layer().read_arrow(options),
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

/// Write an Arrow C Data Interface stream into a layer.
///
/// Writing appends. This is the ABI's only way to change a feature table, and
/// there is no call that updates or deletes an existing feature; a consumer
/// needing row-level update or delete uses the Rust crate, whose
/// `Layer::writer` provides both.
///
/// The stream's schema must name columns the layer has. A column the layer
/// does not have is refused rather than dropped, because discarding data a
/// caller asked to write would be worse than declining it; a column of the
/// layer the stream does not name is left to its default. A type the layer
/// cannot store is `GPKG_STATUS_INVALID_ARGUMENT`. Building the layer with
/// `gpkg_create_layer_from_arrow_schema` from the same schema is what makes
/// the two agree by construction.
///
/// A stream column matching the layer's primary key supplies each row's fid,
/// and a NULL there has one assigned. A stream appending to a layer that
/// already holds rows should therefore omit that column, or carry NULLs in
/// it: a fid the table already holds fails the write.
///
/// Takes ownership of `stream`, as the C Data Interface specifies for a moved
/// stream: it is released here whether the write succeeds or fails, and the
/// caller must not release it again. `out_rows`, when non-NULL, receives the
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
/// `batch_size` is the rows per write transaction; `0` writes everything in
/// one transaction. Inside a transaction the caller opened with `gpkg_begin`
/// it bounds nothing, since every batch belongs to that one, and is then a
/// statement about how many rows are held at once rather than about
/// durability.
///
/// A failure part-way through leaves whatever earlier batches have already
/// committed, so `batch_size` also decides how much a failed write can leave
/// behind: `0` leaves nothing, because there is one transaction. Inside a
/// caller's transaction nothing is durable until their commit, and
/// `gpkg_rollback` discards all of it.
///
/// The layer's spatial index, where it has one, is brought up to date as part
/// of the write.
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

/// Create a layer whose columns come from an Arrow schema.
///
/// The schema is borrowed, not consumed: the caller still owns it and must
/// release it. This is what lets a C consumer copy a layer without any
/// schema-description API of its own, by taking the schema from the source
/// stream's `get_schema` and handing it straight here.
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
/// The geometry column and its spatial reference system are taken from the
/// schema's GeoArrow metadata. The referenced SRS must already exist in the
/// destination; `gpkg_add_epsg_srs` puts one there. The geometry column is
/// declared `GEOMETRY` rather than the source's own type, because the GeoArrow
/// WKB encoding does not carry one.
///
/// `spatial_index` asks for an RTree index over that column, which is the
/// usual choice for a layer that will be queried by bounding box.
/// `gpkg_layer_create_spatial_index` adds one later, and costs a pass over the
/// rows to do it.
///
/// Fails with `GPKG_STATUS_ALREADY_EXISTS` when the file already has a table
/// of that name, and with `GPKG_STATUS_NOT_FOUND` when the schema names a
/// spatial reference system the file does not carry.
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

/// Register an EPSG spatial reference system in the file.
///
/// A layer cannot name an SRS the file does not carry, so this is what a C
/// consumer calls before creating one.
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
