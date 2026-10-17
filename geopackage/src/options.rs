//! Options for opening or creating a [`GeoPackage`].
//!
//! This module holds the [`OpenOptions`] builder and its typed [`JournalMode`]
//! and [`Synchronous`] settings. [`OpenOptions`] mirrors
//! [`std::fs::OpenOptions`]: you build up the settings, then call
//! [`OpenOptions::create`], [`OpenOptions::open`],
//! [`OpenOptions::open_read_only`] or [`OpenOptions::from_connection`]. The
//! plain [`GeoPackage::create`] and [`GeoPackage::open`] constructors are
//! shortcuts that use the default options.
//!
//! No rusqlite types appear here. Journal mode and synchronous level are the
//! crate's own enums, translated to the underlying `PRAGMA` values internally.

use std::path::Path;
use std::time::Duration;

use rusqlite::OpenFlags;

use crate::{GeoPackage, Result};

/// SQLite journal mode for a GeoPackage connection.
///
/// The default favours interchange. If you don't set a journal mode, the file
/// keeps the one it has, and a newly created file uses [`JournalMode::Delete`]
/// (SQLite's own default), so a `.gpkg` you hand over is a single file.
/// [`JournalMode::Wal`] is opt-in, for concurrent read/write throughput. A
/// handle that opted into WAL checkpoints and resets the file to `Delete` on
/// [`GeoPackage::close`] and on drop, so no `-wal`/`-shm` sidecars are left
/// behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalMode {
    /// The rollback journal (`PRAGMA journal_mode = DELETE`). It keeps the
    /// database in a single file, and is the interchange default.
    Delete,
    /// Write-ahead logging (`PRAGMA journal_mode = WAL`). Faster concurrent
    /// read/write, at the cost of `-wal`/`-shm` sidecar files while the
    /// connection is open. A WAL handle is reset to `Delete` on close or drop.
    Wal,
}

impl JournalMode {
    /// Returns the `PRAGMA journal_mode` keyword.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Wal => "WAL",
        }
    }
}

/// SQLite `synchronous` durability level (`PRAGMA synchronous`).
///
/// Trades write throughput against how much recent work a power loss or OS
/// crash can undo. [`Synchronous::Full`] is SQLite's default in rollback-journal
/// mode, and [`Synchronous::Normal`] is the usual choice under
/// [`JournalMode::Wal`]. If you leave it unset, SQLite's own default applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Synchronous {
    /// `PRAGMA synchronous = OFF` (0). No syncing: fastest and least durable.
    Off,
    /// `PRAGMA synchronous = NORMAL` (1).
    Normal,
    /// `PRAGMA synchronous = FULL` (2).
    Full,
    /// `PRAGMA synchronous = EXTRA` (3).
    Extra,
}

impl Synchronous {
    /// Returns the numeric `PRAGMA synchronous` code (0–3).
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::Normal => 1,
            Self::Full => 2,
            Self::Extra => 3,
        }
    }
}

/// How long a statement waits for another connection's lock before failing,
/// unless [`OpenOptions::busy_timeout`] says otherwise.
///
/// SQLite's own default is to fail immediately, so any concurrent writer turns
/// an ordinary write into an error. Five seconds is long enough to wait out the
/// writes a cooperating process makes, and short enough not to look like a
/// hang.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A builder for opening or creating a [`GeoPackage`] with an explicit journal
/// mode and/or `synchronous` level.
///
/// Both settings are optional. If you don't set a journal mode, the file keeps
/// its existing one (a newly created file is [`JournalMode::Delete`]). If you
/// don't set a synchronous level, SQLite's default applies.
/// [`Self::open_read_only`] ignores the journal mode, because it cannot write
/// the change.
///
/// # Examples
///
/// ```no_run
/// # fn main() -> Result<(), geopackage::Error> {
/// use geopackage::{JournalMode, OpenOptions, Synchronous};
/// let gpkg = OpenOptions::new()
///     .journal_mode(JournalMode::Wal)
///     .synchronous(Synchronous::Normal)
///     .create("service.gpkg")?;
/// # let _ = gpkg;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenOptions {
    pub(crate) journal_mode: Option<JournalMode>,
    pub(crate) synchronous: Option<Synchronous>,
    pub(crate) busy_timeout: Option<Duration>,
    pub(crate) mmap_size: Option<u64>,
    pub(crate) allow_unsupported_extension_writes: bool,
    pub(crate) enforce_column_constraints: bool,
    pub(crate) lenient: bool,
    #[cfg(feature = "coverage-samples")]
    pub(crate) max_decoded_samples: Option<usize>,
}

impl OpenOptions {
    /// Creates a builder with no settings applied.
    ///
    /// These are the defaults the plain [`GeoPackage::create`] and
    /// [`GeoPackage::open`] constructors use.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the journal mode ([`JournalMode::Wal`] to opt into WAL).
    #[must_use]
    pub fn journal_mode(mut self, mode: JournalMode) -> Self {
        self.journal_mode = Some(mode);
        self
    }

    /// Accepts legacy and lightly non-conforming files, recording what was
    /// accepted as [`crate::OpenWarning`]s rather than failing to open.
    ///
    /// This combines with every other setting here.
    /// [`GeoPackage::open_lenient`] and [`GeoPackage::open_read_only_lenient`]
    /// take no options at all, so use this to combine leniency with WAL or with
    /// constraint enforcement.
    ///
    /// Leniency covers presentation, not identity. A file that cannot be
    /// identified as a GeoPackage, or that is missing a required core table, is
    /// still an error. Retrieve what was accepted with
    /// [`GeoPackage::open_warnings`].
    #[must_use]
    pub fn lenient(mut self, lenient: bool) -> Self {
        self.lenient = lenient;
        self
    }

    /// Sets the most samples a coverage tile may declare and still be
    /// decoded. The default is
    /// [`DEFAULT_MAX_DECODED_SAMPLES`](geopackage_core::coverage::DEFAULT_MAX_DECODED_SAMPLES),
    /// 4096 by 4096.
    ///
    /// A tile's header states its size, and the decoders allocate for that
    /// size before reading the data. Where a read knows the tile's zoom level,
    /// it first refuses a tile larger than the level declares, as GDAL does;
    /// this limit applies on top of that, to every decode. A tile over it is
    /// not decoded: reads return
    /// [`TileError::PayloadTooLarge`](geopackage_core::TileError::PayloadTooLarge),
    /// and [`GeoPackage::validate`] does not check its samples.
    ///
    /// Raise it for coverages with larger tiles than GDAL writes by default.
    /// Each sample costs eight bytes decoded.
    #[cfg(feature = "coverage-samples")]
    #[must_use]
    pub fn max_decoded_samples(mut self, samples: usize) -> Self {
        self.max_decoded_samples = Some(samples);
        self
    }

    /// Sets the `synchronous` durability level.
    #[must_use]
    pub fn synchronous(mut self, synchronous: Synchronous) -> Self {
        self.synchronous = Some(synchronous);
        self
    }

    /// Sets how long a statement waits for a lock another connection holds
    /// before giving up with `SQLITE_BUSY`. Defaults to
    /// [`DEFAULT_BUSY_TIMEOUT`].
    ///
    /// `Duration::ZERO` restores SQLite's own default, which is to fail
    /// immediately. That is rarely what you want. A GeoPackage is a
    /// single-writer database that readers and writers are expected to share,
    /// so a write that meets a concurrent writer should wait rather than fail
    /// on the first attempt.
    ///
    /// This is the crate's entire retry policy, and SQLite does the waiting.
    /// SQLite skips the wait in the two cases where it could not help: a
    /// read-to-write upgrade that would deadlock under a rollback journal, and
    /// a stale snapshot under WAL. Both return `SQLITE_BUSY` at once, however
    /// long the timeout is.
    #[must_use]
    pub fn busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = Some(timeout);
        self
    }

    /// Reads the file through a memory map of up to `bytes` bytes
    /// (`PRAGMA mmap_size`). `0` turns the map off, which is the default.
    ///
    /// Without a map, SQLite reads each page with a system call that copies it
    /// into its own cache. With one, it reads pages from the map, and makes no
    /// read calls for the mapped part of the file. That makes reads of rows
    /// larger than a page faster, because SQLite reads their overflow pages
    /// one call each. Reads of small rows can be slower. Measure it on your
    /// own files before you turn it on.
    ///
    /// SQLite limits the map to its compile-time `SQLITE_MAX_MMAP_SIZE`:
    /// 2,147,418,112 bytes in a default build, which includes the `bundled`
    /// feature. A larger file is mapped only up to that size, and the rest is
    /// read as before.
    ///
    /// SQLite documents the costs of a memory map. Among them, an I/O error on
    /// a mapped page stops the process with a signal (`SIGBUS` on Unix)
    /// instead of returning an error, so use it only for files on reliable
    /// local storage.
    ///
    /// A threaded [`crate::Layer::read_arrow`] gives its worker connections
    /// the same map size.
    #[must_use]
    pub fn mmap_size(mut self, bytes: u64) -> Self {
        self.mmap_size = Some(bytes);
        self
    }

    /// Allows writes to tables covered by an extension this crate cannot
    /// identify, instead of failing with [`Error::UnsupportedExtension`].
    ///
    /// By default such a write fails. An unidentifiable extension may
    /// constrain the rows, triggers or encodings of the table it covers, and
    /// under Requirement 64 a writer has to understand every extension. Set
    /// this when you know what the extension is and that writing beside it is
    /// safe. The check is the crate's, not the format's, and if you know more
    /// than the catalogue does, you should be able to say so.
    ///
    /// Reads are unaffected either way, and extensions this crate can name
    /// never trigger the error. GDAL, for comparison, warns in this
    /// situation and proceeds.
    ///
    /// [`Error::UnsupportedExtension`]: crate::Error::UnsupportedExtension
    #[must_use]
    pub fn allow_unsupported_extension_writes(mut self, allow: bool) -> Self {
        self.allow_unsupported_extension_writes = allow;
        self
    }

    /// Checks written values against the `gpkg_schema` constraints their
    /// columns declare, rejecting a row that violates one with
    /// [`Error::ColumnConstraintViolation`].
    ///
    /// Off by default, because the format makes these constraints advisory:
    /// "These restrictions MAY be enforced by SQL triggers or by code in
    /// applications that update GeoPackage data values". A file can therefore
    /// contain values its own constraints forbid, and rejecting writes beside
    /// them by default would impose a rule the format does not.
    ///
    /// For a constrained column, a `range` is checked against integers and
    /// floats, and a `glob` by asking SQLite. An `enum` is checked against text
    /// and against the decimal form of a number, since the spec's own sample
    /// enumerates numbers as text. NULL satisfies every constraint, because a
    /// constraint says what a value may be, not that there has to be one. Use
    /// `NOT NULL` for that. Blob, date and datetime values are not checked.
    ///
    /// SQLite matches globs (`SELECT ?1 GLOB ?2`, prepared once per writer),
    /// not this crate. The pattern language has no definition beyond what
    /// SQLite does with it, and this crate bundles SQLite, so the engine that
    /// stores the file decides what its own constraints mean. SQLite also
    /// applies its own text coercion to a number, which is what a trigger
    /// enforcing the same constraint would do.
    ///
    /// This applies to every write path, including the columnar one. The
    /// measured cost is about 31% on a 200,000-row write with two constrained
    /// columns, and nothing for a layer with no constraints.
    ///
    /// [`Error::ColumnConstraintViolation`]: crate::Error::ColumnConstraintViolation
    #[must_use]
    pub fn enforce_column_constraints(mut self, enforce: bool) -> Self {
        self.enforce_column_constraints = enforce;
        self
    }

    /// Fills in [`DEFAULT_BUSY_TIMEOUT`] where the caller set none.
    ///
    /// The entry points that open the connection themselves apply this. A
    /// caller-supplied connection keeps its existing timeout unless the caller
    /// sets one, on the same principle that leaves its journal mode alone.
    pub(crate) fn with_default_busy_timeout(mut self) -> Self {
        if self.busy_timeout.is_none() {
            self.busy_timeout = Some(DEFAULT_BUSY_TIMEOUT);
        }
        self
    }

    /// Creates a new GeoPackage 1.4 file at `path` with these options.
    ///
    /// Like [`GeoPackage::create`], but applies the configured journal mode and
    /// `synchronous` level to the new connection.
    pub fn create<P: AsRef<Path>>(self, path: P) -> Result<GeoPackage> {
        GeoPackage::create_configured(path.as_ref(), self)
    }

    /// Opens an existing GeoPackage read-write with these options.
    pub fn open<P: AsRef<Path>>(self, path: P) -> Result<GeoPackage> {
        GeoPackage::open_configured(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE, self)
    }

    /// Opens an existing GeoPackage read-only with these options.
    ///
    /// The journal mode is ignored, because a read-only connection cannot
    /// change it. The `synchronous` level is still applied.
    pub fn open_read_only<P: AsRef<Path>>(self, path: P) -> Result<GeoPackage> {
        GeoPackage::open_configured(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY, self)
    }

    /// Wraps an already-open connection with these options.
    ///
    /// Like [`GeoPackage::from_connection`], but applies the configured journal
    /// mode and `synchronous` level. If a journal mode is set, you are
    /// responsible for making sure the connection is writable.
    pub fn from_connection(self, conn: rusqlite::Connection) -> Result<GeoPackage> {
        GeoPackage::from_connection_configured(conn, self, true)
    }
}
