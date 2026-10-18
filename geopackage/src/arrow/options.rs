/// Default number of rows per [`RecordBatch`](arrow_array::RecordBatch).
///
/// This is large enough to make per-batch overhead negligible, and small
/// enough that a batch of wide rows stays a sensible allocation. GDAL's driver
/// uses the same default (`MAX_FEATURES_IN_BATCH`).
pub const DEFAULT_BATCH_SIZE: usize = 65_536;

/// The default ceiling on the geometry bytes one [`RecordBatch`](arrow_array::RecordBatch) may contain.
///
/// The geometry column is Arrow `Binary`, whose offsets are `i32`, so a single
/// batch can't address more than 2 GB of WKB. This is a hard limit of the
/// type, not a tuning choice, because a batch that crossed it couldn't be
/// represented at all. A read that would cross it emits a short batch and
/// continues, so the ceiling only affects layers whose geometries are large
/// enough to reach it.
///
/// This is the hard ceiling, and no setting can raise a batch past it. By
/// default a read uses [`default_max_batch_bytes`], which is
/// `min(INT32_MAX, RAM / 4)`. You can set your own with
/// [`ArrowReadOptions::with_max_batch_bytes`].
///
/// The alternative, Arrow `LargeBinary`, has `i64` offsets and no such
/// ceiling. It was rejected for two reasons. It would give every consumer
/// 64-bit offsets to solve a problem that only very large geometries have.
/// `Binary` is also the encoding the ecosystem already exchanges for this
/// data, including GDAL's driver, so these batches need no conversion. The
/// `geoarrow.wkb` encoding permits either.
pub const DEFAULT_MAX_BATCH_BYTES: usize = i32::MAX as usize;

/// Returns the byte ceiling a read uses if you don't set one:
/// `min(INT32_MAX, RAM / 4)`.
///
/// The memory term only matters below about 8 GB of RAM, because a quarter of
/// anything larger already exceeds [`DEFAULT_MAX_BATCH_BYTES`]. It stops a
/// single batch from using more than a quarter of a small machine's memory.
/// GDAL's driver picks its ceiling the same way.
///
/// The system's memory is read once and cached. It is used to choose a
/// default, not to track memory that changes while the program runs.
#[must_use]
pub fn default_max_batch_bytes() -> usize {
    static RESOLVED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let quarter = usize::try_from(system.total_memory() / 4).unwrap_or(usize::MAX);
        // An unavailable reading comes back as 0, which would put one row in
        // every batch. Treat it as no information and keep the fixed ceiling.
        if quarter == 0 {
            DEFAULT_MAX_BATCH_BYTES
        } else {
            quarter.min(DEFAULT_MAX_BATCH_BYTES)
        }
    })
}

/// Ceiling on the automatically chosen thread count.
///
/// Beyond a few readers, the work is limited by how fast SQLite can read pages,
/// not by the number of cores, so more threads don't help. GDAL's driver caps
/// the same path at `min(4, cpus)`.
const DEFAULT_MAX_THREADS: usize = 4;

/// Options for the columnar read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArrowReadOptions {
    /// Rows per [`RecordBatch`](arrow_array::RecordBatch). Defaults to [`DEFAULT_BATCH_SIZE`].
    pub batch_size: usize,
    /// Ceiling on the geometry bytes one [`RecordBatch`](arrow_array::RecordBatch) may contain. Defaults
    /// to [`default_max_batch_bytes`]. A batch that would cross it is emitted
    /// short, and the rows that did not fit begin the next one.
    pub max_batch_bytes: usize,
    /// How many threads may read at once. `0` chooses
    /// `min(4, available parallelism)`, and `1` reads on the calling thread.
    ///
    /// A read uses more than one thread only under the conditions listed in
    /// [`crate::Layer::read_arrow`]. Otherwise it is single-threaded, whatever
    /// this is set to.
    pub threads: usize,
}

impl ArrowReadOptions {
    /// Returns options with an explicit batch size. A size of `0` is raised
    /// to `1`.
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size: batch_size.max(1),
            ..Self::default()
        }
    }

    /// Sets the ceiling on geometry bytes per batch. See
    /// [`DEFAULT_MAX_BATCH_BYTES`] for its purpose and why it can't be raised
    /// past `i32::MAX`.
    ///
    /// Values above `i32::MAX` are clamped to it, because Arrow `Binary` can't
    /// address more than that in one batch.
    #[must_use]
    pub fn with_max_batch_bytes(mut self, max_batch_bytes: usize) -> Self {
        self.max_batch_bytes = max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES);
        self
    }

    /// Sets the thread count. `0` chooses a default, `1` reads on the calling
    /// thread.
    #[must_use]
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// Returns the thread count to use, resolving `0` to the default.
    pub(crate) fn resolved_threads(self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .min(DEFAULT_MAX_THREADS)
    }
}

impl Default for ArrowReadOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            max_batch_bytes: default_max_batch_bytes(),
            threads: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_ceiling_never_exceeds_the_offset_limit() {
        let resolved = default_max_batch_bytes();
        assert!(resolved > 0, "a zero ceiling would put one row in a batch");
        assert!(
            resolved <= DEFAULT_MAX_BATCH_BYTES,
            "i32 offsets cannot address {resolved} bytes"
        );
        // Cached, so a second call cannot disagree with the first.
        assert_eq!(resolved, default_max_batch_bytes());
    }

    #[test]
    fn a_caller_cannot_raise_the_ceiling_past_the_offset_limit() {
        let options = ArrowReadOptions::default().with_max_batch_bytes(usize::MAX);
        assert_eq!(options.max_batch_bytes, DEFAULT_MAX_BATCH_BYTES);
    }
}
