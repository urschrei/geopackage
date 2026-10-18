use arrow_array::RecordBatch;
use arrow_schema::ArrowError;
use geopackage_core::ident::quote;
use rusqlite::Connection;

use crate::{Error, Result};

use super::BatchSource;
use super::options::ArrowReadOptions;

/// One layer's batches read by a pool of worker threads.
///
/// The key span is cut into windows of `batch_size` consecutive keys. Worker
/// `w` of `n` reads windows `w`, `w + n`, `w + 2n`, and so on, and tags each
/// message with its window. A worker that finds a window empty skips ahead to
/// its first window at or after the next key in the table, so a sparse key
/// costs one lookup per gap instead of one query per empty window.
///
/// The consumer merges the workers' messages in window order. It knows the
/// lowest window each worker can still send, and takes a message only when no
/// worker can send an earlier one. Each worker's channel is a rendezvous
/// channel, and the consumer holds at most one received message per worker,
/// so each worker has at most one batch waiting and one being read.
pub(crate) struct ParallelBatches {
    workers: Vec<WorkerHandle>,
    threads: Vec<std::thread::JoinHandle<()>>,
    done: bool,
}

impl ParallelBatches {
    pub(crate) fn spawn(plan: &WorkerPlan) -> Self {
        let mut workers = Vec::with_capacity(plan.threads);
        let mut threads = Vec::with_capacity(plan.threads);
        for worker in 0..plan.threads {
            let (tx, rx) = std::sync::mpsc::sync_channel(0);
            let plan = plan.clone();
            threads.push(std::thread::spawn(move || run_worker(&plan, worker, &tx)));
            workers.push(WorkerHandle {
                receiver: rx,
                held: None,
                floor: worker as u64,
                finished: false,
            });
        }
        Self {
            workers,
            threads,
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
        let threads = self.workers.len() as u64;
        loop {
            // Take what any worker is offering, so that it can start on its
            // next window while earlier windows are passed on.
            for worker in &mut self.workers {
                if worker.held.is_some() || worker.finished {
                    continue;
                }
                match worker.receiver.try_recv() {
                    Ok(Ok(message)) => {
                        worker.floor = message.window();
                        worker.held = Some(message);
                    }
                    Ok(Err(error)) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => worker.finished = true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            // The earliest window among the held messages.
            let held = self
                .workers
                .iter()
                .enumerate()
                .filter_map(|(i, w)| w.held.as_ref().map(|m| (m.window(), i)))
                .min();
            // The worker with nothing held that could still send the lowest
            // window. Its next message has to arrive before a held message
            // for a later window can be passed on.
            let waiting = self
                .workers
                .iter()
                .enumerate()
                .filter(|(_, w)| w.held.is_none() && !w.finished)
                .map(|(i, w)| (w.floor, i))
                .min();
            if let Some((floor, i)) = waiting
                && held.is_none_or(|(window, _)| floor < window)
            {
                let worker = self.workers.get_mut(i)?;
                match worker.receiver.recv() {
                    Ok(Ok(message)) => {
                        worker.floor = message.window();
                        worker.held = Some(message);
                    }
                    Ok(Err(error)) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                    // The worker has read all its windows.
                    Err(_) => worker.finished = true,
                }
                continue;
            }
            let Some((_, i)) = held else {
                self.done = true;
                return None;
            };
            let worker = self.workers.get_mut(i)?;
            match worker.held.take()? {
                WorkerMessage::Batch { batch, .. } => return Some(Ok(batch)),
                WorkerMessage::WindowEnd { window } => {
                    worker.floor = window.saturating_add(threads);
                }
            }
        }
    }
}

impl Drop for ParallelBatches {
    fn drop(&mut self) {
        // Dropping the receivers makes each worker's next send fail, which is
        // how a consumer that stops early tells the pool to stop.
        self.workers.clear();
        for thread in self.threads.drain(..) {
            drop(thread.join());
        }
    }
}

/// The consumer's view of one worker.
struct WorkerHandle {
    receiver: std::sync::mpsc::Receiver<std::result::Result<WorkerMessage, ArrowError>>,
    /// A message received but not yet passed on.
    held: Option<WorkerMessage>,
    /// The lowest window this worker can still send.
    floor: u64,
    finished: bool,
}

/// What the workers share: where the database is and how its key windows are
/// laid out.
#[derive(Clone)]
pub(crate) struct WorkerPlan {
    pub(crate) path: std::path::PathBuf,
    pub(crate) table: String,
    pub(crate) key: String,
    pub(crate) conversion: crate::ConversionOptions,
    pub(crate) first: i64,
    pub(crate) last: i64,
    pub(crate) batch_size: usize,
    pub(crate) max_batch_bytes: usize,
    pub(crate) threads: usize,
    /// The reading connection's map size, which each worker's connection
    /// takes too.
    pub(crate) mmap_size: u64,
}

/// Returns the file behind a connection's `main` database, or `None` if it has
/// no file. A `:memory:` or temporary database has none, and no other
/// connection can open it.
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

/// Returns the smallest and largest value of `key` in `table`, or `None` if
/// the table is empty. Each is one index lookup, because `key` is the
/// table's integer primary key.
pub(crate) fn key_span(conn: &Connection, table: &str, key: &str) -> Result<Option<(i64, i64)>> {
    let (table, key) = (quote(table)?, quote(key)?);
    let first: Option<i64> =
        conn.query_row(&format!("SELECT min({key}) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    let last: Option<i64> =
        conn.query_row(&format!("SELECT max({key}) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    Ok(first.zip(last))
}

/// Runs one worker. It opens its own read-only connection and reads its
/// windows in order, until the layer runs out or the consumer goes away.
#[hotpath::measure(label = "arrow::run_worker")]
fn run_worker(
    plan: &WorkerPlan,
    worker: usize,
    tx: &std::sync::mpsc::SyncSender<std::result::Result<WorkerMessage, ArrowError>>,
) {
    let send_error = |error: Error| {
        drop(tx.send(Err(ArrowError::ExternalError(Box::new(error)))));
    };
    let gpkg = match crate::OpenOptions::new()
        .mmap_size(plan.mmap_size)
        .open_read_only(&plan.path)
    {
        Ok(gpkg) => gpkg,
        Err(error) => return send_error(error),
    };
    let layer = match gpkg.layer(&plan.table) {
        Ok(layer) => layer.with_conversion_options(plan.conversion),
        Err(error) => return send_error(error),
    };
    // Threads of its own would recurse; this reader is the thread.
    let options = ArrowReadOptions::with_batch_size(plan.batch_size)
        .with_threads(1)
        .with_max_batch_bytes(plan.max_batch_bytes);
    let mut batches = match layer.read_arrow(options) {
        Ok(batches) => batches,
        Err(error) => return send_error(error),
    };
    let BatchSource::Sequential(source) = &mut batches.source else {
        return;
    };
    let next_key_sql = match (quote(&plan.table), quote(&plan.key)) {
        (Ok(table), Ok(key)) => format!("SELECT min({key}) FROM {table} WHERE {key} > ?1"),
        (Err(error), _) | (_, Err(error)) => return send_error(error.into()),
    };
    let (width, threads) = (plan.batch_size as u64, plan.threads as u64);

    let mut window = worker as u64;
    loop {
        // The window's keys, or the end of the layer if they are past it.
        let Some(start) = window
            .checked_mul(width)
            .and_then(|offset| i64::try_from(offset).ok())
            .and_then(|offset| plan.first.checked_add(offset))
            .filter(|start| *start <= plan.last)
        else {
            return;
        };
        let end = start
            .saturating_add(i64::try_from(width - 1).unwrap_or(i64::MAX))
            .min(plan.last);
        source.read_window(start, end);
        let mut empty = true;
        for batch in source.by_ref() {
            empty = false;
            let message = batch.map(|batch| WorkerMessage::Batch { window, batch });
            let failed = message.is_err();
            if tx.send(message).is_err() || failed {
                return;
            }
        }
        if tx.send(Ok(WorkerMessage::WindowEnd { window })).is_err() {
            return; // the consumer stopped
        }
        let mut next = window.saturating_add(threads);
        if empty {
            // Skip the empty windows after this one: find the next key, and
            // continue from this worker's first window that can hold it.
            let key: Option<i64> = match gpkg
                .connection()
                .query_row(&next_key_sql, [end], |row| row.get(0))
            {
                Ok(key) => key,
                Err(error) => return send_error(error.into()),
            };
            let Some(key) = key else {
                return;
            };
            let holding = key.abs_diff(plan.first) / width;
            if holding > next {
                // The first window at or after `holding` that this worker owns.
                next = holding + (window + threads - holding % threads) % threads;
            }
        }
        window = next;
    }
}

/// What a worker sends for each window it reads.
///
/// The byte ceiling can split one window into several batches, so a worker
/// marks where each window ends, instead of the consumer assuming one batch
/// per window.
enum WorkerMessage {
    Batch { window: u64, batch: RecordBatch },
    WindowEnd { window: u64 },
}

impl WorkerMessage {
    fn window(&self) -> u64 {
        match self {
            Self::Batch { window, .. } | Self::WindowEnd { window } => *window,
        }
    }
}
