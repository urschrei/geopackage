//! [`WriteTransaction`]: the transaction a write path runs in, either its own
//! or the caller's.
//!
//! # The problem
//!
//! SQLite has no nested transactions. A `BEGIN` issued while a transaction is
//! already open fails with "cannot start a transaction within a transaction".
//! Every write path in this crate used to issue one unconditionally. A caller
//! who had begun a transaction on [`crate::GeoPackage::connection`] and then
//! used the ordinary API got that error instead of their writes. The C ABI
//! couldn't offer `gpkg_begin` and `gpkg_commit` at all, because the sequence a
//! C consumer would reasonably write could not work.
//!
//! # Inheriting, rather than nesting
//!
//! When a transaction is already open, the write path runs its statements in
//! that one. Atomicity is the same: the statements are grouped, they commit
//! together, and a failure rolls them back together. What changes is who
//! decides when that happens: whoever issued the `BEGIN`.
//!
//! [`crate::Layer::extent`] has worked this way since it was written. This type
//! makes that branch reusable.
//!
//! Savepoints are the other way to get nesting, and this crate does not use
//! them for that. `rusqlite::Connection::savepoint` needs `&mut Connection`,
//! but this crate has only `&Connection` (the read path shares it), so
//! savepoints are driven as raw `SAVEPOINT`/`RELEASE` text.
//!
//! [`in_savepoint`] uses one for a smaller unit: a write of several statements
//! inside a longer transaction. A coverage tile is a tile row and an ancillary
//! row. If the second insert fails, the savepoint removes the first, so the
//! transaction stays consistent for a caller who handles the error and
//! continues.
//!
//! # What it costs the caller
//!
//! This has three consequences, each documented on the API it affects:
//!
//! - **`commit` does not commit** when the transaction was inherited. It still
//!   does everything else a commit does, including flushing `gpkg_contents`,
//!   but the caller issues the durable commit.
//! - **Dropping a writer does not roll back** when the transaction was
//!   inherited, so an error part-way through leaves partial work staged in the
//!   caller's transaction for them to discard.
//! - **[`crate::Layer::write_all`]'s `batch_size` stops bounding
//!   transactions**, because every batch belongs to the caller's transaction.

use rusqlite::{Connection, Transaction};

/// The transaction a write path runs in.
///
/// Construct with [`Self::begin`], which picks a variant by asking the
/// connection whether a transaction is already open. Finish with
/// [`Self::commit`], which commits only what it opened.
///
/// This is deliberately not `Deref<Target = Connection>`. Every site that has
/// one also has the `&Connection` it was opened on, and they are the same
/// connection, so a deref would only add a second name for it. Statements go
/// through the connection. This value only decides the commit.
pub(crate) enum WriteTransaction<'conn> {
    /// Opened here, and so committed or rolled back here.
    Owned(Transaction<'conn>),
    /// The caller's transaction, joined rather than nested. The caller commits
    /// or rolls it back.
    Inherited,
}

impl<'conn> WriteTransaction<'conn> {
    /// Opens a transaction on `conn`, or inherits the one already open.
    ///
    /// Uses `unchecked_transaction` rather than `transaction`, because the
    /// latter needs `&mut Connection` and this crate shares `&Connection`
    /// between the read and write paths.
    pub(crate) fn begin(conn: &'conn Connection) -> rusqlite::Result<Self> {
        if conn.is_autocommit() {
            Ok(Self::Owned(conn.unchecked_transaction()?))
        } else {
            Ok(Self::Inherited)
        }
    }

    /// Commits, if there is anything here to commit.
    ///
    /// An inherited transaction is left open and the call returns success. The
    /// work stays staged in the caller's transaction. Committing here would
    /// also commit the caller's own statements, and that is the caller's
    /// decision.
    pub(crate) fn commit(self) -> rusqlite::Result<()> {
        match self {
            Self::Owned(tx) => tx.commit(),
            Self::Inherited => Ok(()),
        }
    }
}

/// Runs `body` in a savepoint named `name` on `conn`. Rolls back to the
/// savepoint if `body` fails, so the statements of a failed `body` leave
/// nothing in the transaction.
///
/// # Errors
///
/// The error of `body`, or the error of the `SAVEPOINT` or `RELEASE`
/// statement.
pub(crate) fn in_savepoint<T, E: From<rusqlite::Error>>(
    conn: &Connection,
    name: &str,
    body: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match body() {
        Ok(value) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(value)
        }
        Err(error) => {
            // Some failures, such as SQLITE_FULL, end the whole transaction
            // and the savepoint with it. The rollback then fails, and the
            // error of `body` is the one to report.
            let _rolled_back = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WriteTransaction, in_savepoint};
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

    /// Pins the failure this type prevents, so that a future change to `begin`
    /// that stops checking fails here rather than in a write path.
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
    /// caller rolls back. This is the other half of the contract.
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

    /// A failed body leaves nothing, and the statements before the savepoint
    /// stay in the transaction.
    #[test]
    fn a_failed_body_rolls_back_to_the_savepoint() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER NOT NULL)")
            .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        conn.execute("INSERT INTO t (v) VALUES (1)", []).unwrap();
        let result: rusqlite::Result<()> = in_savepoint(&conn, "test_put", || {
            conn.execute("INSERT INTO t (v) VALUES (2)", [])?;
            conn.execute("INSERT INTO t (v) VALUES (NULL)", [])?;
            Ok(())
        });
        assert!(result.is_err());
        in_savepoint(&conn, "test_put", || {
            conn.execute("INSERT INTO t (v) VALUES (3)", []).map(|_| ())
        })
        .unwrap();
        tx.commit().unwrap();
        let values: Vec<i64> = conn
            .prepare("SELECT v FROM t ORDER BY v")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(values, vec![1, 3]);
    }
}
