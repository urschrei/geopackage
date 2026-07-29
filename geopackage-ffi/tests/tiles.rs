//! Tile pyramids through the C entry points.

use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

use geopackage_ffi::{
    Status, gpkg_bytes_free, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_open,
    gpkg_open_read_only, gpkg_string_free, gpkg_t, gpkg_tiles_count, gpkg_tiles_count_at,
    gpkg_tiles_delete, gpkg_tiles_extent, gpkg_tiles_free, gpkg_tiles_get, gpkg_tiles_has,
    gpkg_tiles_matrix_at, gpkg_tiles_name, gpkg_tiles_open, gpkg_tiles_put, gpkg_tiles_t,
    gpkg_tiles_zoom_level_count,
};

fn error_slot() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Ok,
        message: std::ptr::null_mut(),
    }
}

fn message(error: &gpkg_error_t) -> Option<String> {
    if error.message.is_null() {
        return None;
    }
    // SAFETY: the library filled this in and has not freed it.
    Some(
        unsafe { CStr::from_ptr(error.message) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn take_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the pointer came from the library and is freed immediately after.
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: the pointer came from the library and is not used again.
    unsafe { gpkg_string_free(ptr) };
    Some(text)
}

fn fixture() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_tiles.gpkg")
}

/// The fixture, read-only.
fn open() -> (*mut gpkg_t, gpkg_error_t) {
    let path = CString::new(fixture().to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

/// A writable copy of the fixture.
fn open_copy(dir: &std::path::Path) -> (*mut gpkg_t, gpkg_error_t) {
    let path = dir.join("t.gpkg");
    std::fs::copy(fixture(), &path).expect("copy the fixture");
    let c_path = CString::new(path.to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

fn pyramid(gpkg: *mut gpkg_t, error: &mut gpkg_error_t) -> *mut gpkg_tiles_t {
    let name = CString::new("tiles").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let handle = unsafe { gpkg_tiles_open(gpkg, name.as_ptr(), &raw mut *error) };
    assert!(!handle.is_null(), "{:?}", message(error));
    handle
}

#[test]
fn a_pyramid_reports_its_name_grid_and_extent() {
    let (gpkg, mut error) = open();
    let tiles = pyramid(gpkg, &mut error);

    // SAFETY: a live pyramid handle.
    let name = unsafe { gpkg_tiles_name(tiles, &raw mut error) };
    assert_eq!(take_string(name).as_deref(), Some("tiles"));

    let mut count = 0i64;
    // SAFETY: a live pyramid handle and a writable out-parameter.
    let status = unsafe { gpkg_tiles_count(tiles, &raw mut count, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(count, 1);

    let mut levels = 0usize;
    // SAFETY: as above.
    let status = unsafe { gpkg_tiles_zoom_level_count(tiles, &raw mut levels, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(levels, 1);

    let (mut zoom, mut mw, mut mh, mut tw, mut th) = (0i64, 0i64, 0i64, 0i64, 0i64);
    // SAFETY: a live pyramid handle and five writable out-parameters.
    let status = unsafe {
        gpkg_tiles_matrix_at(
            tiles,
            0,
            &raw mut zoom,
            &raw mut mw,
            &raw mut mh,
            &raw mut tw,
            &raw mut th,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok);
    assert_eq!((zoom, mw, mh, tw, th), (0, 1, 1, 256, 256));

    let mut srs = 0i32;
    // SAFETY: a live pyramid handle; the coordinate out-parameters are NULL,
    // which is documented as "do not write this one".
    let status = unsafe {
        gpkg_tiles_extent(
            tiles,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut srs,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok);
    assert_eq!(srs, 3857);

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_tile_comes_back_as_the_bytes_the_file_holds() {
    let (gpkg, mut error) = open();
    let tiles = pyramid(gpkg, &mut error);

    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len = 0usize;
    // SAFETY: a live pyramid handle and two writable out-parameters.
    let status =
        unsafe { gpkg_tiles_get(tiles, 0, 0, 0, &raw mut data, &raw mut len, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(!data.is_null());
    assert!(len > 0);

    // SAFETY: the library wrote `len` readable bytes at `data`.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    // Undecoded: what comes out is the stored PNG, magic bytes and all.
    assert_eq!(&bytes[..4], b"\x89PNG");
    // SAFETY: the buffer came from `gpkg_tiles_get` with this length, and is
    // freed exactly once.
    unsafe { gpkg_bytes_free(data, len) };

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn an_absent_tile_is_not_found_rather_than_an_error() {
    let (gpkg, mut error) = open();
    let tiles = pyramid(gpkg, &mut error);

    let mut has = true;
    // SAFETY: a live pyramid handle and a writable out-parameter.
    let status = unsafe { gpkg_tiles_has(tiles, 0, 5, 5, &raw mut has, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert!(!has);

    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len = 99usize;
    // SAFETY: as above, with two writable out-parameters.
    let status =
        unsafe { gpkg_tiles_get(tiles, 0, 5, 5, &raw mut data, &raw mut len, &raw mut error) };
    assert_eq!(status, Status::NotFound);
    // A sparse pyramid is ordinary, so the out-parameters are left in a state
    // the caller can act on rather than untouched.
    assert!(data.is_null());
    assert_eq!(len, 0);

    // Freeing a NULL buffer does nothing, so a caller need not branch.
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_bytes_free(std::ptr::null_mut(), 0) };

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_tile_can_be_written_read_back_and_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());
    let tiles = pyramid(gpkg, &mut error);

    // Take the fixture's own tile and write it to a new address, so the
    // payload is one the pyramid's declared tile size accepts.
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len = 0usize;
    // SAFETY: a live pyramid handle and two writable out-parameters.
    let status =
        unsafe { gpkg_tiles_get(tiles, 0, 0, 0, &raw mut data, &raw mut len, &raw mut error) };
    assert_eq!(status, Status::Ok);
    // SAFETY: the library wrote `len` readable bytes at `data`.
    let payload = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    // SAFETY: freed exactly once, with the length the call reported.
    unsafe { gpkg_bytes_free(data, len) };

    // The fixture's grid is 1x1 at zoom 0, so 0/0/0 is the only valid address;
    // overwriting it is what there is room to test.
    // SAFETY: a live pyramid handle and a readable payload.
    let status = unsafe {
        gpkg_tiles_put(
            tiles,
            0,
            0,
            0,
            payload.as_ptr(),
            payload.len(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));

    let mut count = 0i64;
    // SAFETY: a live pyramid handle and a writable out-parameter.
    let status = unsafe { gpkg_tiles_count_at(tiles, 0, &raw mut count, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(count, 1, "an overwrite replaces rather than adds");

    // And it deletes.
    let mut deleted = false;
    // SAFETY: a live pyramid handle and a writable out-parameter.
    let status = unsafe { gpkg_tiles_delete(tiles, 0, 0, 0, &raw mut deleted, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert!(deleted);

    // SAFETY: as above.
    let status = unsafe { gpkg_tiles_delete(tiles, 0, 0, 0, &raw mut deleted, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert!(!deleted, "deleting what is not there is not a failure");

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_live_pyramid_handle_blocks_a_close() {
    // The same rule as layer handles and streams: a pyramid holds an erased
    // borrow of its container, so it counts against the same tally.
    let (gpkg, mut error) = open();
    let tiles = pyramid(gpkg, &mut error);

    // SAFETY: a live container handle; the close is expected to be refused.
    let refused = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(refused, Status::HandleInUse);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container now.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_pyramid_that_is_not_there_reports_rather_than_returning_a_handle() {
    let (gpkg, mut error) = open();
    let name = CString::new("nope").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let missing = unsafe { gpkg_tiles_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(missing.is_null());
    assert_ne!(error.code, Status::Ok);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a failed open took no token, so nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn null_handles_are_rejected_throughout() {
    let mut error = error_slot();
    let mut count = 0i64;

    // SAFETY: NULL is the case under test throughout.
    let status = unsafe { gpkg_tiles_count(std::ptr::null(), &raw mut count, &raw mut error) };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: as above.
    let name = unsafe { gpkg_tiles_name(std::ptr::null(), &raw mut error) };
    assert!(name.is_null());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: freeing NULL is explicitly permitted.
    unsafe { gpkg_tiles_free(std::ptr::null_mut()) };
}

#[test]
fn the_caller_buffer_reader_reports_the_size_it_needs() {
    let (gpkg, mut error) = open();
    let tiles = pyramid(gpkg, &mut error);

    // Too small on purpose: the call should refuse, write nothing, and still
    // say how much room the tile wants.
    let mut small = [0u8; 4];
    let mut len = 0usize;
    // SAFETY: a live pyramid handle, a writable buffer of the stated size, and
    // a writable out-parameter.
    let status = unsafe {
        geopackage_ffi::gpkg_tiles_get_into(
            tiles,
            0,
            0,
            0,
            small.as_mut_ptr(),
            small.len(),
            &raw mut len,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::InvalidArgument);
    assert!(
        len > small.len(),
        "out_len should hold the size needed: {len}"
    );
    assert_eq!(small, [0u8; 4], "nothing should have been written");
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // Now with room. No allocation crosses the boundary, so there is nothing
    // to free and no length to get wrong.
    let mut buffer = vec![0u8; len];
    let mut written = 0usize;
    // SAFETY: as above, with a buffer of the reported size.
    let status = unsafe {
        geopackage_ffi::gpkg_tiles_get_into(
            tiles,
            0,
            0,
            0,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut written,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(written, len);
    assert_eq!(&buffer[..4], b"\x89PNG");

    // And an absent tile is still Not Found rather than a failure.
    // SAFETY: as above.
    let status = unsafe {
        geopackage_ffi::gpkg_tiles_get_into(
            tiles,
            0,
            5,
            5,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut written,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::NotFound);
    assert_eq!(written, 0);

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

/// A PNG header declaring `width` by `height`, which is as much as the payload
/// probe reads. Enough to test what the pyramid does with the dimensions,
/// without carrying a second image fixture for the sake of its size.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR payload length
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 2, 0, 0, 0]); // depth, colour, compression, filter, interlace
    bytes.extend_from_slice(&[0, 0, 0, 0]); // CRC, which the probe does not check
    bytes
}

#[test]
fn a_rejected_tile_reports_a_category_rather_than_other() {
    // Tile failures used to reach C as GPKG_STATUS_OTHER, because the library
    // error that carries them is one variant wrapping an enum of its own. A
    // caller can now branch on them.
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());
    let tiles = pyramid(gpkg, &mut error);

    // The fixture's grid is 1x1 at zoom 0, so column 1 is off it. The address
    // is the caller's, so this is an argument that was rejected.
    let payload = png_header(256, 256);
    // SAFETY: a live pyramid handle and a readable payload.
    let status = unsafe {
        gpkg_tiles_put(
            tiles,
            0,
            1,
            0,
            payload.as_ptr(),
            payload.len(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::InvalidArgument, "{:?}", message(&error));
    assert!(
        message(&error).is_some_and(|text| text.contains("outside")),
        "the message should still say what was wrong"
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // Bytes that are not an image at all, at an address that is fine.
    let junk = b"this is not an image";
    // SAFETY: as above.
    let status =
        unsafe { gpkg_tiles_put(tiles, 0, 0, 0, junk.as_ptr(), junk.len(), &raw mut error) };
    assert_eq!(status, Status::InvalidArgument, "{:?}", message(&error));
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // A readable image whose dimensions are not the ones the zoom level
    // declares. The argument is well formed; it violates a rule the pyramid
    // states about itself, so this is a constraint rather than a bad argument.
    let wrong_size = png_header(64, 64);
    // SAFETY: as above.
    let status = unsafe {
        gpkg_tiles_put(
            tiles,
            0,
            0,
            0,
            wrong_size.as_ptr(),
            wrong_size.len(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Constraint, "{:?}", message(&error));
    assert!(
        message(&error).is_some_and(|text| text.contains("64")),
        "the message should name the size that was offered"
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live pyramid handle, freed exactly once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}
