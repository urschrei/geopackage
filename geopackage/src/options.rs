//! Connection options for opening or creating a [`GeoPackage`]: the
//! [`OpenOptions`] builder and its typed [`JournalMode`] and [`Synchronous`]
//! settings.
//!
//! [`OpenOptions`] mirrors [`std::fs::OpenOptions`]: build up the settings, then
//! call a terminal [`OpenOptions::create`], [`OpenOptions::open`],
//! [`OpenOptions::open_read_only`], or [`OpenOptions::from_connection`]. The
//! plain [`GeoPackage::create`] / [`GeoPackage::open`] constructors are the
//! default-options shortcuts.
//!
//! No rusqlite types appear here: journal mode and synchronous level are the
//! crate's own enums, translated to the underlying `PRAGMA` values internally.

use std::path::Path;
use std::time::Duration;

use rusqlite::OpenFlags;

use crate::{GeoPackage, Result};

/// SQLite journal mode for a GeoPackage connection.
///
/// The default is interchange-first: an unspecified journal mode leaves the
/// file as it is, and a file created fresh is [`JournalMode::Delete`] (SQLite's
/// own default), so a handed-over `.gpkg` is a single file. [`JournalMode::Wal`]
/// is opt-in for concurrent read/write throughput; a handle that opted into WAL
/// checkpoints and resets the file to `Delete` on [`GeoPackage::close`] and on
/// drop, so no `-wal`/`-shm` sidecars are left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalMode {
    /// The rollback journal (`PRAGMA journal_mode = DELETE`). Single-file, the
    /// interchange default.
    Delete,
    /// Write-ahead logging (`PRAGMA journal_mode = WAL`). Faster concurrent
    /// read/write, at the cost of `-wal`/`-shm` sidecar files while the
    /// connection is open; a WAL handle is reset to `Delete` on close/drop.
    Wal,
}

impl JournalMode {
    /// The `PRAGMA journal_mode` keyword.
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
/// mode; [`Synchronous::Normal`] is the usual choice under
/// [`JournalMode::Wal`]. Leaving it unspecified keeps SQLite's own default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Synchronous {
    /// `PRAGMA synchronous = OFF` (0): no syncing; fastest, least durable.
    Off,
    /// `PRAGMA synchronous = NORMAL` (1).
    Normal,
    /// `PRAGMA synchronous = FULL` (2).
    Full,
    /// `PRAGMA synchronous = EXTRA` (3).
    Extra,
}

impl Synchronous {
    /// The numeric `PRAGMA synchronous` code (0–3).
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::Normal => 1,
            Self::Full => 2,
            Self::Extra => 3,
        }
    }
}

/// A builder for opening or creating a [`GeoPackage`] with an explicit journal
/// mode and/or `synchronous` level.
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
///
/// How long a statement waits for another connection's lock before failing,
/// unless [`OpenOptions::busy_timeout`] says otherwise.
///
/// SQLite's own default is to fail immediately, which makes any concurrent
/// writer turn an ordinary write into an error. Five seconds is long enough to
/// ride out the writes a cooperating process actually makes and short enough
/// not to look like a hang.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Both settings are optional. An unspecified journal mode leaves the file's
/// existing mode untouched (a freshly created file is
/// [`JournalMode::Delete`]); an unspecified synchronous level keeps SQLite's
/// default. Journal mode is ignored by [`Self::open_read_only`], which cannot
/// write the change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenOptions {
    pub(crate) journal_mode: Option<JournalMode>,
    pub(crate) synchronous: Option<Synchronous>,
    pub(crate) busy_timeout: Option<Duration>,
    pub(crate) allow_unsupported_extension_writes: bool,
}

impl OpenOptions {
    /// A builder with no settings applied (the same defaults the plain
    /// [`GeoPackage::create`] / [`GeoPackage::open`] constructors use).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the journal mode ([`JournalMode::Wal`] to opt into WAL).
    #[must_use]
    pub fn journal_mode(mut self, mode: JournalMode) -> Self {
        self.journal_mode = Some(mode);
        self
    }

    /// Set the `synchronous` durability level.
    #[must_use]
    pub fn synchronous(mut self, synchronous: Synchronous) -> Self {
        self.synchronous = Some(synchronous);
        self
    }

    /// How long a statement waits for a lock another connection holds before
    /// giving up with `SQLITE_BUSY`. [`DEFAULT_BUSY_TIMEOUT`] when unset.
    ///
    /// `Duration::ZERO` restores SQLite's own default, which is to fail
    /// immediately. That is rarely what a caller wants: a GeoPackage is a
    /// single-writer database that readers and writers are expected to share,
    /// so a write that meets a concurrent writer should wait rather than fail
    /// on the first attempt.
    ///
    /// This is the whole of the crate's retry policy. Waiting is what SQLite
    /// does on a caller's behalf here, and it is worth knowing that the wait is
    /// skipped in the two cases where it could not help: a read-to-write
    /// upgrade that would deadlock under a rollback journal, and a stale
    /// snapshot under WAL, both of which return `SQLITE_BUSY` at once however
    /// long the timeout is.
    #[must_use]
    pub fn busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = Some(timeout);
        self
    }

    /// Write to tables carrying an extension this crate cannot identify,
    /// instead of refusing with [`Error::UnsupportedExtension`].
    ///
    /// By default a write to such a table is refused, because an extension we
    /// cannot name may constrain the rows, triggers or encodings of the table
    /// it covers, and Requirement 64 makes every extension one a writer has to
    /// understand. Set this when you know what the extension is and know that
    /// writing beside it is safe: the check is the crate's, not the format's,
    /// and a caller who knows more than the catalogue does should be able to
    /// say so.
    ///
    /// Reads are unaffected either way, and extensions this crate can name
    /// never trigger the refusal. GDAL, for comparison, warns in this
    /// situation and proceeds.
    ///
    /// [`Error::UnsupportedExtension`]: crate::Error::UnsupportedExtension
    #[must_use]
    pub fn allow_unsupported_extension_writes(mut self, allow: bool) -> Self {
        self.allow_unsupported_extension_writes = allow;
        self
    }

    /// Fill in [`DEFAULT_BUSY_TIMEOUT`] where the caller set none.
    ///
    /// Applied by the entry points that open the connection themselves. A
    /// caller-supplied connection keeps whatever timeout it already has unless
    /// the caller asks for one, on the same principle that leaves its journal
    /// mode alone.
    pub(crate) fn with_default_busy_timeout(mut self) -> Self {
        if self.busy_timeout.is_none() {
            self.busy_timeout = Some(DEFAULT_BUSY_TIMEOUT);
        }
        self
    }

    /// Create a new GeoPackage 1.4 file at `path` with these options.
    ///
    /// As [`GeoPackage::create`], but applying the configured journal mode and
    /// `synchronous` level to the new connection.
    pub fn create<P: AsRef<Path>>(self, path: P) -> Result<GeoPackage> {
        GeoPackage::create_configured(path.as_ref(), self)
    }

    /// Open an existing GeoPackage read-write with these options.
    pub fn open<P: AsRef<Path>>(self, path: P) -> Result<GeoPackage> {
        GeoPackage::open_configured(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE, self)
    }

    /// Open an existing GeoPackage read-only with these options.
    ///
    /// The journal mode is ignored (a read-only connection cannot change it);
    /// the `synchronous` level is still applied.
    pub fn open_read_only<P: AsRef<Path>>(self, path: P) -> Result<GeoPackage> {
        GeoPackage::open_configured(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY, self)
    }

    /// Wrap an already-open connection with these options.
    ///
    /// As [`GeoPackage::from_connection`], but applying the configured journal
    /// mode and `synchronous` level. The caller is responsible for the
    /// connection being writable if a journal mode is set.
    pub fn from_connection(self, conn: rusqlite::Connection) -> Result<GeoPackage> {
        GeoPackage::from_connection_configured(conn, self, true)
    }
}
