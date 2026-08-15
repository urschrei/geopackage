use arrow_array::RecordBatch;
use arrow_schema::ArrowError;
use geopackage_core::ident::quote;
use rusqlite::Connection;

use crate::{Error, Result};

use super::BatchSource;
use super::options::ArrowReadOptions;

/// The file backing a connection's `main` database, or `None` for one with no
/// file: a `:memory:` or temporary database, which no other connection can open.
pub(crate) fn database_path(conn: &Connection) -> Result<Option<std::path::PathBuf>> {
    let file: String = conn.query_row(
        "SELECT file FROM pragma_database_list WHERE name = 'main'",
        [],
        |row| row.get(0),
    )?;
    if file.is_empty() {
        return Ok(None);
    }
    Ok(Some(std::path::PathBuf::from(file)))
}

/// The `(first, last)` primary key of `table` when the key has no gaps, or
/// `None` when it has gaps or the table is empty.
///
/// Density is what lets a worker be handed a key range before any row is read
/// and know how many rows it covers. Testing `max - min + 1 == count` is a
/// slightly wider rule than GDAL's `min == 1 && max == count`, and costs the
/// same single scan.
pub(crate) fn dense_key_span(
    conn: &Connection,
    table: &str,
    key: &str,
) -> Result<Option<(i64, i64)>> {
    let sql = format!(
        "SELECT min({key}), max({key}), count(*) FROM {table}",
        key = quote(key)?,
        table = quote(table)?
    );
    let (min, max, count): (Option<i64>, Option<i64>, i64) =
        conn.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(None);
    };
    let span = max.checked_sub(min).and_then(|d| d.checked_add(1));
    if span != Some(count) {
        return Ok(None);
    }
    Ok(Some((min, max)))
}

/// One layer's batches read by a pool of worker threads.
///
/// Worker `w` of `n` reads batches `w`, `w + n`, `w + 2n`, and so on, and the
/// consumer takes from the workers in the same rotation. Batches therefore
/// arrive in key order without any reordering buffer, and each worker's channel
/// buffers one batch, so the memory in flight is bounded by the thread count.
pub(crate) struct ParallelBatches {
    /// One receiver per worker, drained in rotation.
    receivers: Vec<std::sync::mpsc::Receiver<std::result::Result<WorkerMessage, ArrowError>>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Which worker to take from next.
    turn: usize,
    done: bool,
}

impl ParallelBatches {
    pub(crate) fn spawn(
        path: std::path::PathBuf,
        table: String,
        conversion: crate::ConversionOptions,
        (first, last): (i64, i64),
        batch_size: usize,
        max_batch_bytes: usize,
        threads: usize,
    ) -> Self {
        let mut receivers = Vec::with_capacity(threads);
        let mut workers = Vec::with_capacity(threads);
        for worker in 0..threads {
            // A capacity of one keeps a worker at most one batch ahead of the
            // consumer, which is what bounds the memory in flight.
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let path = path.clone();
            let table = table.clone();
            let handle = std::thread::spawn(move || {
                run_worker(
                    &path,
                    &table,
                    conversion,
                    first,
                    last,
                    batch_size,
                    max_batch_bytes,
                    threads,
                    worker,
                    &tx,
                );
            });
            receivers.push(rx);
            workers.push(handle);
        }
        Self {
            receivers,
            workers,
            turn: 0,
            done: false,
        }
    }
}

impl Iterator for ParallelBatches {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            match self.receivers.get(self.turn)?.recv() {
                Ok(Ok(WorkerMessage::Batch(batch))) => return Some(Ok(batch)),
                // This worker's window is done, so the next window in key
                // order belongs to the next worker.
                Ok(Ok(WorkerMessage::WindowEnd)) => {
                    self.turn = (self.turn + 1) % self.receivers.len().max(1);
                }
                Ok(Err(error)) => {
                    self.done = true;
                    return Some(Err(error));
                }
                // The worker whose turn it is has finished, and workers are
                // assigned windows in rotation, so every later worker has
                // finished too.
                Err(_) => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

impl Drop for ParallelBatches {
    fn drop(&mut self) {
        // Dropping the receivers makes each worker's next send fail, which is
        // how a consumer that stops early tells the pool to stop.
        self.receivers.clear();
        for worker in self.workers.drain(..) {
            drop(worker.join());
        }
    }
}

/// One worker: open a read-only connection of its own and read the batches
/// assigned to it, in order, until the layer runs out or the consumer goes away.
#[expect(
    clippy::too_many_arguments,
    reason = "a worker's whole context, passed once at spawn; a struct would be used by this call site alone"
)]
#[hotpath::measure(label = "arrow::run_worker")]
fn run_worker(
    path: &std::path::Path,
    table: &str,
    conversion: crate::ConversionOptions,
    first: i64,
    last: i64,
    batch_size: usize,
    max_batch_bytes: usize,
    threads: usize,
    worker: usize,
    tx: &std::sync::mpsc::SyncSender<std::result::Result<WorkerMessage, ArrowError>>,
) {
    let send_error = |error: Error| {
        drop(tx.send(Err(ArrowError::ExternalError(Box::new(error)))));
    };
    let gpkg = match crate::GeoPackage::open_read_only(path) {
        Ok(gpkg) => gpkg,
        Err(error) => return send_error(error),
    };
    let layer = match gpkg.layer(table) {
        Ok(layer) => layer.with_conversion_options(conversion),
        Err(error) => return send_error(error),
    };
    // Threads of its own would recurse; this reader is the thread.
    let options = ArrowReadOptions::with_batch_size(batch_size)
        .with_threads(1)
        .with_max_batch_bytes(max_batch_bytes);
    let mut batches = match layer.read_arrow(options) {
        Ok(batches) => batches,
        Err(error) => return send_error(error),
    };
    let BatchSource::Sequential(source) = &mut batches.source else {
        return;
    };

    let stride = match i64::try_from(batch_size.saturating_mul(threads)) {
        Ok(stride) if stride > 0 => stride,
        _ => return,
    };
    let start = match i64::try_from(batch_size.saturating_mul(worker)) {
        Ok(offset) => match first.checked_add(offset) {
            Some(start) => start,
            None => return,
        },
        Err(_) => return,
    };

    let mut key = start;
    while key <= last {
        // One window of `batch_size` rows, which is usually one batch. The
        // byte ceiling can split it into several, and every one of them
        // belongs to this worker: skipping to the next window on a short batch
        // would drop the rows that did not fit.
        let mut remaining = batch_size;
        let mut at = key;
        while remaining > 0 {
            match source.read_batch_at(at, remaining) {
                Ok(Some(batch)) => {
                    let rows = source.last_batch_rows;
                    if tx.send(Ok(WorkerMessage::Batch(batch))).is_err() {
                        return; // the consumer stopped
                    }
                    if rows == 0 {
                        break;
                    }
                    remaining -= rows.min(remaining);
                    at = source.next_key;
                }
                // The layer ends inside this window, so it ends for this
                // worker too: every later window starts beyond it.
                Ok(None) => return,
                Err(error) => return send_error(error),
            }
        }
        if tx.send(Ok(WorkerMessage::WindowEnd)).is_err() {
            return; // the consumer stopped
        }
        match key.checked_add(stride) {
            Some(next) => key = next,
            None => return,
        }
    }
}

/// What a worker sends for each window it reads.
///
/// Batches arrive in key order because each worker owns a fixed, repeating
/// slice of the key space and the consumer takes their windows in rotation.
/// The byte ceiling can split one window into several batches, so a worker
/// marks where its window ends rather than the consumer assuming one batch per
/// turn.
enum WorkerMessage {
    Batch(RecordBatch),
    WindowEnd,
}
