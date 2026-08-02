/// Default number of rows per [`RecordBatch`](arrow_array::RecordBatch).
///
/// The same default GDAL's driver uses (`MAX_FEATURES_IN_BATCH`). Large enough
/// that per-batch overhead disappears, small enough that a batch of wide rows
/// stays a sensible allocation.
pub const DEFAULT_BATCH_SIZE: usize = 65_536;

/// The default ceiling on the geometry bytes one [`RecordBatch`](arrow_array::RecordBatch) may contain.
///
/// The geometry column is Arrow `Binary`, whose offsets are `i32`, so a single
/// batch cannot address more than 2 GB of WKB. This is a hard limit of the
/// type rather than a tuning choice: a batch that crossed it could not be
/// represented at all. A read that would cross it emits a short batch and
/// continues, so the ceiling costs nothing except on layers whose geometries
/// are large enough to reach it.
///
/// This is the hard ceiling, and no setting can raise a batch past it. The
/// default a read actually uses is [`default_max_batch_bytes`], which follows
/// GDAL in taking `min(INT32_MAX, RAM / 4)`; a caller can set its own with
/// [`ArrowReadOptions::with_max_batch_bytes`].
///
/// The alternative was Arrow `LargeBinary`, whose `i64` offsets have no such
/// ceiling. It was rejected because it would give every consumer 64-bit
/// offsets to solve a problem only very large geometries have, and because
/// matching GDAL keeps these batches interchangeable with the encoding the
/// ecosystem already reads. The `geoarrow.wkb` encoding permits either.
pub const DEFAULT_MAX_BATCH_BYTES: usize = i32::MAX as usize;

/// The byte ceiling a read uses when the caller sets none: GDAL's
/// `min(INT32_MAX, RAM / 4)`.
///
/// The memory term only binds below about 8 GB of RAM, since a quarter of
/// anything larger already exceeds [`DEFAULT_MAX_BATCH_BYTES`]. It is there so
/// a small machine does not spend a quarter of itself on a single batch.
///
/// The system's memory is read once and cached. It is queried to choose a
/// default, not to track a machine whose memory changes while we run.
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
/// GDAL's driver uses the same `min(4, cpus)` for the same path. Beyond a few
/// readers the work is bounded by how fast SQLite can pull pages rather than by
/// cores.
const DEFAULT_MAX_THREADS: usize = 4;

/// Options for the columnar read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArrowReadOptions {
    /// Rows per [`RecordBatch`](arrow_array::RecordBatch). Defaults to [`DEFAULT_BATCH_SIZE`].
    pub batch_size: usize,
    /// Ceiling on the geometry bytes one [`RecordBatch`](arrow_array::RecordBatch) may contain. Defaults
    /// to [`DEFAULT_MAX_BATCH_BYTES`]. A batch that would cross it is emitted
    /// short, and the rows that did not fit begin the next one.
    pub max_batch_bytes: usize,
    /// How many threads may read at once. `0` chooses
    /// `min(4, available parallelism)`, matching GDAL's default for the same
    /// path; `1` reads on the calling thread.
    ///
    /// More than one thread is only possible under the conditions in
    /// [`crate::Layer::read_arrow`]; when they do not hold, the read is single-threaded
    /// whatever this says.
    pub threads: usize,
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

impl ArrowReadOptions {
    /// Options with an explicit batch size. A size of `0` is raised to `1`.
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size: batch_size.max(1),
            ..Self::default()
        }
    }

    /// Sets the ceiling on geometry bytes per batch. See
    /// [`DEFAULT_MAX_BATCH_BYTES`] for its purpose and why it cannot simply
    /// be raised past `i32::MAX`.
    ///
    /// Values above `i32::MAX` are clamped to it, since Arrow `Binary` cannot
    /// address more than that within one batch whatever the caller asks for.
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

    /// The thread count to actually use, resolving `0` to the default.
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
