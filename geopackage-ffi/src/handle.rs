//! The handle representation, and the one piece of lifetime erasure the crate
//! rests on.
//!
//! # What a C caller sees
//!
//! Three opaque pointers, each with one destructor, and one rule between them:
//! a container cannot be closed while anything taken from it is still alive.
//! The attempt returns `GPKG_STATUS_HANDLE_IN_USE`, changes nothing, and leaves
//! the container open, so the caller can free the children and try again.
//!
//! ```c
//! gpkg_error_t error = {GPKG_STATUS_OK, NULL};
//! gpkg_t *gpkg = gpkg_open_read_only("places.gpkg", &error);
//! gpkg_layer_t *layer = gpkg_layer_open(gpkg, "points", &error);
//! gpkg_tiles_t *tiles = gpkg_tiles_open(gpkg, "basemap", &error);
//!
//! if (gpkg_close(gpkg, &error) == GPKG_STATUS_HANDLE_IN_USE) {
//!     // The message counts the outstanding children. Nothing was torn down:
//!     // every handle above, gpkg included, is still usable.
//!     gpkg_error_clear(&error);
//! }
//!
//! gpkg_tiles_free(tiles);
//! gpkg_layer_free(layer);
//! gpkg_close(gpkg, &error);
//! ```
//!
//! An Arrow stream from `gpkg_layer_read_arrow` counts as a child in the same
//! way, and is freed through its own `release` callback rather than by a
//! `gpkg_*_free` call. Freeing a child twice, or using one after it has been
//! freed, is undefined behaviour: the rule protects the container from its
//! children, not each handle from its own caller.
//!
//! # The problem it solves
//!
//! `geopackage::Layer<'a>` borrows the `GeoPackage` it came from, and so do
//! `TilePyramid<'a>` and the cursors. A C caller holds independent pointers and
//! frees them in whatever order it likes, so a borrow cannot be expressed
//! across the boundary.
//!
//! The alternative to erasing it is for each call to rebuild the borrowed
//! handle from its parent, which was measured rather than assumed. Building one
//! costs a near-constant 37 to 51 microseconds, since it is a fixed set of
//! catalogue queries, and it is constant whatever the call it precedes costs:
//! +778% on `get_tile`, and +131% to +150% on a small bounding-box query. So
//! the borrow is erased once, when the handle is made, and the cost of a call
//! from C is the cost of the call.
//!
//! # The shape
//!
//! A container handle owns its `GeoPackage` behind a `Box`, so the address of
//! the `GeoPackage` is fixed for as long as the handle lives, whatever happens
//! to the handle itself. A child handle stores a `Layer<'static>` produced by
//! erasing the borrow, plus a pointer back to its parent so it can announce its
//! own death.
//!
//! # Why that is sound
//!
//! Three invariants, all enforced here rather than assumed of the caller:
//!
//! 1. **The borrowed-from value never moves.** `Container::gpkg` is a `Box`,
//!    and nothing in this crate takes the `GeoPackage` out of it or hands out a
//!    `&mut` to it. Moving the `Container` itself, which cannot happen anyway
//!    once it is behind a raw pointer, would not move the `GeoPackage`.
//! 2. **A child never outlives its parent.** The only path that destroys a
//!    container checks [`Container::outstanding_children`] first and refuses
//!    while it is non-zero; freeing a child decrements it. So the `GeoPackage`
//!    a `Layer<'static>` points into is still alive whenever that layer is
//!    used.
//! 3. **Nothing crosses a thread.** `geopackage::GeoPackage` is `Send` but not
//!    `Sync`, because `rusqlite::Connection` is, so a handle belongs to the
//!    thread that made it. The ABI documents handle-per-thread, and the counter
//!    below is a plain `Cell` on that basis rather than an atomic, which would
//!    imply a guarantee this crate does not make.
//!
//! Invariant 2 is the one a C caller sees, as the refusal in the example
//! above. The Rust API keeps its compile-time version, since `close` there
//! takes `self` and a live `Layer` borrows it.

use std::cell::Cell;

use geopackage::{
    BoundingBox, FeatureWriter, GeoPackage, Layer, Tile, TileCursor, TilePyramid, TileStream,
};

/// A `gpkg_t`: an open GeoPackage, and a count of the handles borrowing it.
pub struct Container {
    /// Boxed so its address is stable. Never replaced, never moved out of, and
    /// never lent mutably, which is invariant 1 above.
    gpkg: Box<GeoPackage>,
    /// Live child handles. Non-zero blocks [`Self::close`].
    ///
    /// A `Cell`, not an `AtomicUsize`: the handles are not `Sync` and the ABI
    /// documents handle-per-thread, so an atomic here would suggest a
    /// cross-thread guarantee that the rest of the type cannot keep.
    children: Cell<usize>,
}

impl Container {
    /// Wrap an open GeoPackage as a container handle.
    pub fn new(gpkg: GeoPackage) -> Self {
        Self {
            gpkg: Box::new(gpkg),
            children: Cell::new(0),
        }
    }

    /// The GeoPackage, borrowed for as long as the caller holds the container.
    pub fn gpkg(&self) -> &GeoPackage {
        &self.gpkg
    }

    /// Open a layer as a child handle, registering it against this container.
    ///
    /// # Errors
    ///
    /// Whatever [`GeoPackage::layer`] returns.
    pub fn layer(&self, name: &str) -> geopackage::Result<LayerHandle> {
        let layer = self.gpkg.layer(name)?;
        Ok(self.adopt(layer))
    }

    /// Open an attribute layer as a child handle.
    ///
    /// # Errors
    ///
    /// Whatever [`GeoPackage::attributes`] returns.
    pub fn attributes(&self, name: &str) -> geopackage::Result<LayerHandle> {
        let layer = self.gpkg.attributes(name)?;
        Ok(self.adopt(layer))
    }

    /// Open a layer projected to the named columns, as a child handle.
    ///
    /// `attributes` picks which open runs underneath, since the two entry
    /// points differ only there.
    ///
    /// # Errors
    ///
    /// Whatever the open returns, or [`geopackage::Error::NoSuchColumn`] from
    /// the projection.
    pub fn layer_with_columns(
        &self,
        name: &str,
        columns: &[&str],
        attributes: bool,
    ) -> geopackage::Result<LayerHandle> {
        let layer = if attributes {
            self.gpkg.attributes(name)?
        } else {
            self.gpkg.layer(name)?
        };
        Ok(self.adopt(layer.with_columns(columns)?))
    }

    /// Erase a layer's borrow and count it as a child.
    ///
    /// The lifetime `'_` on the way in is a borrow of `*self.gpkg`, which lives
    /// as long as `self` and does not move. On the way out it is `'static`,
    /// which claims more than the borrow can support on its own; what supports
    /// it is the module's invariants. The layer is only reachable through a
    /// [`LayerHandle`], and dropping that handle is what decrements the counter
    /// that keeps `self` alive.
    fn adopt(&self, layer: Layer<'_>) -> LayerHandle {
        // SAFETY: `Layer<'a>` borrows the `GeoPackage` inside `self.gpkg`,
        // which is boxed and never moved or replaced (invariant 1), and this
        // container cannot be closed while the returned handle is alive
        // (invariant 2, enforced by `close` checking `children`). So the
        // erased-to-`'static` borrow never outlives what it points at. The
        // transmute changes only the lifetime parameter; `Layer` has no other
        // generic parameter and the same layout either way.
        let erased: Layer<'static> = unsafe { std::mem::transmute(layer) };
        LayerHandle {
            layer: erased,
            _token: self.token(),
        }
    }

    /// Open a tile pyramid as a child handle, registering it against this
    /// container.
    ///
    /// # Errors
    ///
    /// Whatever [`GeoPackage::tiles`] returns.
    pub fn tiles(&self, name: &str) -> geopackage::Result<TilesHandle> {
        let pyramid = self.gpkg.tiles(name)?;
        // SAFETY: the same argument as `adopt`. `TilePyramid<'a>` borrows the
        // `GeoPackage` inside `self.gpkg`, which is boxed and never moved or
        // replaced (invariant 1), and the token taken below stops this
        // container closing while the handle lives (invariant 2). The
        // transmute changes only the lifetime parameter.
        let erased: TilePyramid<'static> = unsafe { std::mem::transmute(pyramid) };
        Ok(TilesHandle {
            pyramid: erased,
            _token: self.token(),
        })
    }

    /// Create a tile pyramid and hand it back as a child handle.
    ///
    /// # Errors
    ///
    /// Whatever [`GeoPackage::create_tile_pyramid`] returns.
    pub fn create_tiles(
        &self,
        builder: &geopackage::TilePyramidBuilder,
    ) -> geopackage::Result<TilesHandle> {
        let pyramid = self.gpkg.create_tile_pyramid(builder)?;
        // SAFETY: the same argument as `tiles` above: the pyramid borrows the
        // boxed, never-moved `GeoPackage`, and the token stops this container
        // closing while the handle lives. The transmute changes only the
        // lifetime parameter.
        let erased: TilePyramid<'static> = unsafe { std::mem::transmute(pyramid) };
        Ok(TilesHandle {
            pyramid: erased,
            _token: self.token(),
        })
    }

    /// Take one count against this container, released when the token drops.
    ///
    /// Used by anything that erases a borrow of this container: layer handles,
    /// and the Arrow streams in [`crate::stream`].
    pub fn token(&self) -> ChildToken {
        self.children.set(self.children.get() + 1);
        ChildToken {
            parent: std::ptr::from_ref(self),
        }
    }

    /// Record that one child handle has gone.
    fn release_child(&self) {
        self.children.set(self.children.get().saturating_sub(1));
    }

    /// How many child handles still borrow this container, if any.
    ///
    /// Checked before [`Self::close`] consumes anything, so a refusal leaves
    /// the container open and usable rather than half-torn-down.
    pub fn outstanding_children(&self) -> usize {
        self.children.get()
    }

    /// Close the container.
    ///
    /// The caller must have checked [`Self::outstanding_children`] first: this
    /// consumes the container, so there is nothing to hand back on refusal.
    ///
    /// # Errors
    ///
    /// Whatever [`GeoPackage::close`] returns, which for a WAL handle is the
    /// checkpoint-and-reset that makes the file a single file again.
    pub fn close(self) -> geopackage::Result<()> {
        self.gpkg.close()
    }
}

/// One count against a container's child tally, released on drop.
///
/// A layer handle holds one. So does an Arrow stream, which borrows the same
/// container and needs the same protection: while a token is alive, the
/// container refuses to close, so an erased `'static` borrow cannot outlive
/// what it points at.
pub struct ChildToken {
    /// The container counted against. Raw rather than a reference, because a
    /// reference would reintroduce the borrow this exists to erase.
    parent: *const Container,
}

impl Drop for ChildToken {
    fn drop(&mut self) {
        // SAFETY: `parent` came from `std::ptr::from_ref` on a live `Container`
        // in `Container::token`, and the container outlives every token it
        // made, because closing refuses while any is outstanding. Tokens are
        // not `Send`, so this runs on the container's own thread.
        let parent = unsafe { &*self.parent };
        parent.release_child();
    }
}

/// A `gpkg_layer_t`: a layer, and the container it borrows.
pub struct LayerHandle {
    /// The erased borrow. See [`Container::adopt`] for why it is sound.
    layer: Layer<'static>,
    /// What keeps the container alive while this handle exists. Dropped with
    /// the handle, which is what later permits a close.
    _token: ChildToken,
}

impl LayerHandle {
    /// The layer, borrowed for as long as the caller holds the handle.
    pub fn layer(&self) -> &Layer<'static> {
        &self.layer
    }

    /// Begin a write transaction over this layer as a child handle.
    ///
    /// No borrow is erased here, unlike the layer and pyramid handles.
    /// `Layer::writer`
    /// returns a `FeatureWriter` carrying the layer's own lifetime parameter
    /// rather than a borrow of `&self`, and this layer's parameter is already
    /// `'static`, so the writer arrives erased. What it still needs is the
    /// count: the writer points into the same container, so the container must
    /// not close while it lives.
    ///
    /// # Errors
    ///
    /// Whatever [`geopackage::Layer::writer`] returns, which for a read-only
    /// container is a refusal.
    pub fn writer(&self) -> geopackage::Result<WriterHandle> {
        let writer = self.layer.writer()?;
        Ok(WriterHandle {
            writer,
            _token: self.token(),
        })
    }

    /// Take another count against this handle's container, for something that
    /// borrows the same container independently, such as an Arrow stream.
    pub fn token(&self) -> ChildToken {
        // SAFETY: this handle holds a token of its own, so the container it
        // points at is still alive; taking a second count against it is the
        // same operation `Container::token` performs.
        let parent = unsafe { &*self._token.parent };
        parent.token()
    }
}

/// A `gpkg_writer_t`: a write transaction over a layer, and the container it
/// borrows.
///
/// Held by value rather than boxed, because unlike a container nothing borrows
/// *this*: the writer is the leaf of the handle graph.
pub struct WriterHandle {
    /// Already `'static` when it arrives, since it carries its layer's lifetime
    /// parameter rather than a borrow of the layer handle. See
    /// [`LayerHandle::writer`].
    writer: FeatureWriter<'static>,
    /// What keeps the container alive while this handle exists.
    _token: ChildToken,
}

impl WriterHandle {
    /// The writer, mutably, which is what every staging call needs.
    pub fn writer_mut(&mut self) -> &mut FeatureWriter<'static> {
        &mut self.writer
    }

    /// Commit the transaction, consuming the handle.
    ///
    /// The token drops with `self` whatever the outcome, so a failed commit
    /// still releases the container rather than leaving it uncloseable.
    ///
    /// # Errors
    ///
    /// Whatever [`geopackage::FeatureWriter::commit`] returns.
    pub fn commit(self) -> geopackage::Result<()> {
        self.writer.commit()
    }
}

/// A `gpkg_tiles_t`: a tile pyramid, and the container it borrows.
pub struct TilesHandle {
    /// The erased borrow. See [`Container::tiles`] for why it is sound.
    pyramid: TilePyramid<'static>,
    /// What keeps the container alive while this handle exists. Dropped with
    /// the handle, which is what later permits a close.
    _token: ChildToken,
}

/// Which stored-tile scan a cursor runs.
pub enum CursorScan {
    /// Every stored tile, in matrix order.
    All,
    /// One zoom level.
    At(i64),
    /// One zoom level, within a bounding box in the pyramid's own SRS.
    In(i64, BoundingBox),
}

impl TilesHandle {
    /// The pyramid, borrowed for as long as the caller holds the handle.
    pub fn pyramid(&self) -> &TilePyramid<'static> {
        &self.pyramid
    }

    /// Take another count against this handle's container, for something that
    /// borrows the same container independently, such as a tile cursor.
    pub fn token(&self) -> ChildToken {
        // SAFETY: this handle holds a token of its own, so the container it
        // points at is still alive; taking a second count against it is the
        // same operation `Container::token` performs.
        let parent = unsafe { &*self._token.parent };
        parent.token()
    }

    /// Begin a stored-tile scan as a child handle.
    ///
    /// The cursor visits what the pyramid stores rather than probing the
    /// declared grid, which on a sparse pyramid is the difference between
    /// O(stored) and O(grid). It counts against the *container*, not against
    /// this handle: the statement underneath borrows the container's
    /// connection, so the tiles handle itself may be freed while the cursor
    /// lives, and the container still refuses to close until the cursor is
    /// freed.
    ///
    /// # Errors
    ///
    /// Whatever the corresponding [`TilePyramid`] cursor constructor returns.
    pub fn cursor(&self, scan: &CursorScan) -> geopackage::Result<TileCursorHandle> {
        let cursor = match scan {
            CursorScan::All => self.pyramid.cursor()?,
            CursorScan::At(zoom) => self.pyramid.cursor_at(*zoom)?,
            CursorScan::In(zoom, bbox) => self.pyramid.cursor_in(*zoom, *bbox)?,
        };
        // SAFETY: the cursor's borrow is a prepared statement against the
        // container's connection, which is boxed and never moved or replaced
        // (invariant 1), and the container cannot close while the token below
        // is alive (invariant 2). So the erased-to-`'static` borrow never
        // outlives what it points at; the transmute changes only the lifetime
        // parameter.
        let erased: TileCursor<'static> = unsafe { std::mem::transmute(cursor) };
        let mut cursor = Box::new(erased);
        let stream = cursor.tiles()?;
        // SAFETY: the stream borrows the boxed cursor's statement. The box
        // gives that statement a stable address, the struct below owns both
        // and declares the stream first, so it drops before the cursor, and
        // nothing hands out a second borrow of the cursor while the stream
        // lives. The transmute changes only the lifetime parameter.
        let stream: TileStream<'static> = unsafe { std::mem::transmute(stream) };
        Ok(TileCursorHandle {
            stream,
            _cursor: cursor,
            _token: self.token(),
        })
    }
}

/// A `gpkg_tile_cursor_t`: one scan over a pyramid's stored tiles.
///
/// Owns the whole borrow chain: the stream borrows the boxed cursor, the
/// cursor's statement borrows the container's connection, and the token is
/// what stops the container closing underneath both. Field order matters:
/// the stream is declared first so it drops before the cursor it borrows.
pub struct TileCursorHandle {
    /// The scan in progress. Erased; borrows `_cursor`.
    stream: TileStream<'static>,
    /// The prepared statement the stream walks. Boxed so its address is
    /// stable while the stream borrows it; never touched again directly.
    _cursor: Box<TileCursor<'static>>,
    /// What keeps the container alive while this handle exists.
    _token: ChildToken,
}

impl TileCursorHandle {
    /// The next stored tile, or `None` at the end of the scan.
    ///
    /// The returned [`Tile`] lends the row's payload: it is valid until the
    /// next call on this handle, exactly as the Rust lending cursor's borrow
    /// rules state, and the C contract repeats.
    ///
    /// # Errors
    ///
    /// Whatever [`TileStream::next`] returns.
    #[expect(
        clippy::should_implement_trait,
        reason = "a lending cursor cannot implement Iterator: its item borrows the iterator. The name matches TileStream::next, whose expectation records the same reasoning"
    )]
    pub fn next(&mut self) -> geopackage::Result<Option<Tile<'_>>> {
        self.stream.next()
    }
}
