//! [`WriteTransaction`]: the transaction a write path runs in, whether it
//! opened one or joined the caller's.
//!
//! # The problem
//!
//! SQLite has no nested transactions. A `BEGIN` issued while one is already
//! open fails with "cannot start a transaction within a transaction", and every
//! write path in this crate used to issue one unconditionally. So a caller who
//! had begun a transaction on [`crate::GeoPackage::connection`] and then used
//! the ordinary API got that error rather than their writes, and the C ABI
//! could not offer `gpkg_begin` and `gpkg_commit` at all, because the sequence a
//! C consumer would reasonably write could not work.
//!
//! # Inheriting, rather than nesting
//!
//! When a transaction is already open, the write path runs its statements in
//! that one. The atomicity is the same: the statements are grouped, they commit
//! together, and a failure rolls them back together. What changes is who decides
//! when that happens, which passes to whoever issued the `BEGIN`.
//!
//! [`crate::Layer::extent`] has worked this way since it was written, and this
//! type is that branch made reusable rather than a new idea.
//!
//! Savepoints are the other way to get nesting, and they are rejected here.
//! `rusqlite::Connection::savepoint` needs `&mut Connection`, and this crate
//! holds only `&Connection` (the read path shares it), so savepoints would have
//! to be driven as raw `SAVEPOINT`/`RELEASE` text. That is more machinery for a
//! finer rollback granularity nobody has asked for. If someone does, this is the
//! one type that would have to change.
//!
//! # What it costs the caller
//!
//! Three consequences follow, and each is documented at the API that carries
//! it:
//!
//! - **`commit` does not commit** when the transaction was inherited. It still
//!   does everything else a commit does, including flushing `gpkg_contents`,
//!   but the durable commit is the caller's to issue.
//! - **Dropping a writer does not roll back** when the transaction was
//!   inherited, so an error part-way through leaves partial work staged in the
//!   caller's transaction for them to discard.
//! - **[`crate::Layer::write_all`]'s `batch_size` stops bounding
//!   transactions**, because every batch belongs to the caller's.

use rusqlite::{Connection, Transaction};

/// The transaction a write path runs in.
///
/// Construct with [`Self::begin`], which decides between the two variants by
/// asking the connection whether a transaction is already open. Finish with
/// [`Self::commit`], which commits only what it opened.
///
/// This is deliberately not `Deref<Target = Connection>`. Every site that holds
/// one also holds the `&Connection` it was opened on, and the two are the same
/// connection, so a deref would offer a second name for something already in
/// scope. Statements are issued against the connection; this value exists only
/// to decide the commit.
pub(crate) enum WriteTransaction<'conn> {
    /// Opened here, and so committed or rolled back here.
    Owned(Transaction<'conn>),
    /// The caller's, joined rather than nested. Committing and rolling back are
    /// theirs.
    Inherited,
}

impl<'conn> WriteTransaction<'conn> {
    /// Open a transaction on `conn`, or inherit the one already open.
    ///
    /// `unchecked_transaction` rather than `transaction`, because the latter
    /// needs `&mut Connection` and this crate shares `&Connection` between the
    /// read and write paths.
    pub(crate) fn begin(conn: &'conn Connection) -> rusqlite::Result<Self> {
        if conn.is_autocommit() {
            Ok(Self::Owned(conn.unchecked_transaction()?))
        } else {
            Ok(Self::Inherited)
        }
    }

    /// Commit, if there is anything here to commit.
    ///
    /// An inherited transaction is left open and reports success: the work is
    /// staged in the caller's transaction, and committing here would commit
    /// their statements alongside ours, which is not this call's to decide.
    pub(crate) fn commit(self) -> rusqlite::Result<()> {
        match self {
            Self::Owned(tx) => tx.commit(),
            Self::Inherited => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WriteTransaction;
    use rusqlite::Connection;

    #[test]
    fn an_idle_connection_yields_an_owned_transaction() {
        let conn = Connection::open_in_memory().unwrap();
        let tx = WriteTransaction::begin(&conn).unwrap();
        assert!(matches!(tx, WriteTransaction::Owned(_)));
        // Opening one leaves the connection in a transaction, which is what the
        // next test detects.
        assert!(!conn.is_autocommit());
        tx.commit().unwrap();
        assert!(conn.is_autocommit());
    }

    #[test]
    fn an_open_transaction_is_inherited() {
        let conn = Connection::open_in_memory().unwrap();
        let outer = conn.unchecked_transaction().unwrap();
        let tx = WriteTransaction::begin(&conn).unwrap();
        assert!(matches!(tx, WriteTransaction::Inherited));
        tx.commit().unwrap();
        // The point of the variant: the commit above returned success and the
        // caller's transaction is still open for them to finish.
        assert!(!conn.is_autocommit());
        outer.commit().unwrap();
        assert!(conn.is_autocommit());
    }

    /// The failure this type exists to prevent, pinned so that a future change
    /// to `begin` that stops checking is caught here rather than in a write
    /// path.
    #[test]
    fn nesting_is_what_sqlite_refuses() {
        let conn = Connection::open_in_memory().unwrap();
        let _outer = conn.unchecked_transaction().unwrap();
        let err = conn.unchecked_transaction().unwrap_err();
        assert!(
            err.to_string().contains("within a transaction"),
            "unexpected error: {err}"
        );
    }

    /// Work staged through an inherited transaction is discarded when the
    /// caller rolls back, which is the other half of the contract.
    #[test]
    fn the_caller_owns_the_rollback() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        let outer = conn.unchecked_transaction().unwrap();
        let tx = WriteTransaction::begin(&conn).unwrap();
        conn.execute("INSERT INTO t (v) VALUES (1)", []).unwrap();
        tx.commit().unwrap();
        outer.rollback().unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "an inherited commit made the row durable");
    }
}
