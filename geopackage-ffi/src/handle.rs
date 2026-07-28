//! The handle representation, and the one piece of lifetime erasure the crate
//! rests on.
//!
//! # The problem
//!
//! `geopackage::Layer<'a>` borrows the `GeoPackage` it came from, and so do
//! `TilePyramid<'a>` and the cursors. A C caller holds independent pointers and
//! frees them in whatever order it likes, so a borrow cannot be expressed
//! across the boundary. The measurements in
//! `roadmap/benchmarks/2026-07-28-handle-construction.md` rule out the obvious
//! alternative, which is for each call to rebuild the borrowed handle from its
//! parent: that costs a near-constant 37 to 51 microseconds, which is +778% on
//! `get_tile` and +131% to +150% on a small bounding-box query.
//!
//! # The shape
//!
//! A container handle owns its `GeoPackage` behind a `Box`, so the address of
//! the `GeoPackage` is fixed for as long as the handle lives, whatever happens
//! to the handle itself. A child handle stores a `Layer<'static>` produced by
//! erasing the real borrow, plus a pointer back to its parent so it can
//! announce its own death.
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
//!    thread that made it. The header documents handle-per-thread, and the
//!    counter below is a plain `Cell` on that basis rather than an atomic,
//!    which would imply a guarantee this crate does not make.
//!
//! Invariant 2 is the one a C caller can feel: closing a container while a
//! layer handle from it is alive fails rather than corrupting anything. That is
//! a runtime check because C has no other kind; the Rust API keeps its
//! compile-time version, since `close` there still takes `self` and a live
//! `Layer` still borrows it.

use std::cell::Cell;

use geopackage::{GeoPackage, Layer};

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

    /// Erase a layer's borrow and count it as a child.
    ///
    /// The lifetime `'_` on the way in is a borrow of `*self.gpkg`, which lives
    /// as long as `self` and does not move. On the way out it is `'static`,
    /// which is a lie the module's invariants make safe: the layer is only
    /// reachable through a [`LayerHandle`], and dropping that handle is what
    /// decrements the counter that keeps `self` alive.
    fn adopt(&self, layer: Layer<'_>) -> LayerHandle {
        // SAFETY: `Layer<'a>` borrows the `GeoPackage` inside `self.gpkg`,
        // which is boxed and never moved or replaced (invariant 1), and this
        // container cannot be closed while the returned handle is alive
        // (invariant 2, enforced by `close` checking `children`). So the
        // erased-to-`'static` borrow never outlives what it points at. The
        // transmute changes only the lifetime parameter; `Layer` has no other
        // generic parameter and the same layout either way.
        let erased: Layer<'static> = unsafe { std::mem::transmute(layer) };
        self.children.set(self.children.get() + 1);
        LayerHandle {
            layer: erased,
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

/// A `gpkg_layer_t`: a layer, and the container it borrows.
pub struct LayerHandle {
    /// The erased borrow. See [`Container::adopt`] for why it is sound.
    layer: Layer<'static>,
    /// The container this borrows, so dropping the handle can decrement its
    /// child count. Raw rather than a reference because a reference would
    /// reintroduce exactly the borrow this type exists to erase.
    parent: *const Container,
}

impl LayerHandle {
    /// The layer, borrowed for as long as the caller holds the handle.
    pub fn layer(&self) -> &Layer<'static> {
        &self.layer
    }
}

impl Drop for LayerHandle {
    fn drop(&mut self) {
        // SAFETY: `parent` came from `std::ptr::from_ref` on a live `Container`
        // in `adopt`, and the container outlives every handle it made, because
        // `close` refuses while any is outstanding (invariant 2) and a
        // container is otherwise only dropped through that path. So the pointer
        // is still valid here. Handles are not `Send`, so this runs on the
        // thread that created the container (invariant 3).
        let parent = unsafe { &*self.parent };
        parent.release_child();
    }
}
