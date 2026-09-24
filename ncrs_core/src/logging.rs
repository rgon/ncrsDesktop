//! Non-blocking log output for the `ncrs` daemon.
//!
//! `env_logger` writes each record to stderr on the thread that logs it. The
//! daemon logs from `fuser-0`, its one FUSE dispatch thread, and from every
//! pool worker, and stderr is a file, a pipe or journald: when that sink is
//! slow (an ext4 journal commit on a busy disk parked `fuser-0` in `write(2)`
//! for 1.7 s in the walker harness, 2026-09-25), every FUSE request of the
//! mount waits behind one log line.
//!
//! So [`init`] keeps env_logger's filter (`RUST_LOG`, else the default given)
//! and its format, but hands each formatted record to a bounded queue instead
//! of stderr. The `log` service writes the queue out. A record that finds the
//! queue full is dropped and counted, and the writer reports the count in one
//! line once it catches up: a caller never waits for the sink. [`flush`]
//! waits, bounded, for what is queued to be written, for the exit paths.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Records the queue holds before a new one is dropped.
pub const QUEUE_LINES: usize = 8192;

/// The shared side of the queue: what callers push into and what `flush`
/// waits on.
pub struct LogQueue {
    tx: SyncSender<Vec<u8>>,
    /// Pushed and not yet written.
    pending: Mutex<usize>,
    written: Condvar,
    /// Dropped since the writer last reported it.
    unreported: AtomicU64,
    /// Dropped over the queue's lifetime.
    dropped: AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

impl LogQueue {
    /// A queue of `capacity` records and the receiving end its writer drains.
    pub fn new(capacity: usize) -> (Arc<LogQueue>, Receiver<Vec<u8>>) {
        let (tx, rx) = sync_channel(capacity);
        let q = LogQueue { tx, pending: Mutex::new(0), written: Condvar::new(), unreported: AtomicU64::new(0), dropped: AtomicU64::new(0), closed: Default::default() };
        (Arc::new(q), rx)
    }

    /// Queues one record, or drops it when the queue is full. Never blocks
    /// on the writer (the `pending` lock is only ever held for a counter update).
    pub fn push(&self, line: Vec<u8>) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        match self.tx.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.unreported.fetch_add(1, Ordering::Relaxed);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.done(1);
            }
        }
    }

    /// Ends `drain` once it has written what was queued before this. For
    /// tests: the daemon's writer runs until the process exits.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Vec::new());
    }

    /// Records dropped because the queue was full, over its lifetime.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn done(&self, n: usize) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *p = p.saturating_sub(n);
        if *p == 0 {
            self.written.notify_all();
        }
    }

    /// Waits until everything pushed so far is written, or `timeout` passes.
    /// True when the queue drained.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        while *p > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            p = match self.written.wait_timeout(p, deadline - now) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        true
    }

    /// The writer loop: writes every queued record to `out`, in order, a
    /// batch per write, until every sender is gone. After a batch, reports
    /// the records dropped meanwhile through `report_drops` (the `log` macros
    /// in the daemon, so the report is formatted like any other line).
    pub fn drain(&self, rx: Receiver<Vec<u8>>, mut out: impl Write, report_drops: impl Fn(u64)) {
        let mut batch = Vec::new();
        while let Ok(first) = rx.recv() {
            // A record is never empty (`write_all` skips an empty buffer): `close`.
            if first.is_empty() && self.closed.load(Ordering::SeqCst) {
                return;
            }
            batch.clear();
            batch.extend_from_slice(&first);
            let mut n = 1;
            while batch.len() < 64 * 1024 {
                match rx.try_recv() {
                    Ok(line) if !line.is_empty() => {
                        batch.extend_from_slice(&line);
                        n += 1;
                    }
                    Ok(_) => {
                        // `close` arrived behind this batch: write it, then stop.
                        let _ = out.write_all(&batch).and_then(|_| out.flush());
                        self.done(n);
                        return;
                    }
                    Err(_) => break,
                }
            }
            // Nowhere to report a failing sink; the lines are gone either way.
            let _ = out.write_all(&batch).and_then(|_| out.flush());
            let lost = self.unreported.swap(0, Ordering::Relaxed);
            if lost > 0 {
                report_drops(lost);
            }
            self.done(n);
        }
    }
}

/// The `env_logger` pipe target: one `write` per formatted record (env_logger
/// hands a record over in a single `write_all`), queued.
struct QueueWriter(Arc<LogQueue>);

impl Write for QueueWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.push(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

static QUEUE: OnceLock<Arc<LogQueue>> = OnceLock::new();

/// Installs the daemon's logger: env_logger's filter (`RUST_LOG`, else
/// `default_filter`) and format, written to stderr by the `log` service. If
/// that service cannot start, env_logger writes to stderr directly, as before.
pub fn init(default_filter: &str) {
    let env = env_logger::Env::default().default_filter_or(default_filter);
    let (queue, rx) = LogQueue::new(QUEUE_LINES);
    let writer = queue.clone();
    let started = crate::bg::spawn_service("log", move || {
        writer.drain(rx, std::io::stderr(), |n| log::warn!("{} log lines dropped: stderr could not keep up", n));
    });
    let mut builder = env_logger::Builder::from_env(env);
    if started.is_ok() {
        builder.target(env_logger::Target::Pipe(Box::new(QueueWriter(queue.clone()))));
        let _ = QUEUE.set(queue);
    }
    builder.init();
}

/// Waits, at most `timeout`, for queued log lines to reach stderr. For exit
/// paths, which end the process without joining the `log` service.
pub fn flush(timeout: Duration) {
    if let Some(q) = QUEUE.get() {
        q.flush(timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// A sink that keeps what it is given. With a gate, its first write says
    /// so on `entered` and then parks until the gate opens: a stuck stderr.
    struct Sink {
        got: Arc<Mutex<Vec<u8>>>,
        gate: Option<(std::sync::mpsc::Sender<()>, Receiver<()>)>,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some((entered, gate)) = self.gate.take() {
                let _ = entered.send(());
                let _ = gate.recv();
            }
            self.got.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stuck_sink_never_blocks_a_caller_and_the_overflow_is_counted() {
        let (q, rx) = LogQueue::new(4);
        let (release, gate) = channel();
        let (entered_tx, entered) = channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::new(AtomicU64::new(0));
        let (took, dropped, flushed_while_stuck, flushed) = std::thread::scope(|s| {
            let sink = Sink { got: got.clone(), gate: Some((entered_tx, gate)) };
            let (qd, rep) = (q.clone(), reported.clone());
            s.spawn(move || qd.drain(rx, sink, |n| {
                rep.fetch_add(n, Ordering::Relaxed);
            }));
            // The writer is inside its first write, holding one record.
            q.push(b"first\n".to_vec());
            entered.recv().unwrap();
            let t = Instant::now();
            let callers: Vec<_> = (0..4)
                .map(|c| {
                    let q = q.clone();
                    s.spawn(move || {
                        for i in 0..250 {
                            q.push(format!("{c}-{i}\n").into_bytes());
                        }
                    })
                })
                .collect();
            for c in callers {
                c.join().unwrap();
            }
            let took = t.elapsed();
            let dropped = q.dropped();
            let flushed_while_stuck = q.flush(Duration::from_millis(50));
            release.send(()).unwrap();
            let flushed = q.flush(Duration::from_secs(5));
            q.close();
            (took, dropped, flushed_while_stuck, flushed)
        });
        assert!(took < Duration::from_secs(2), "callers waited on the sink: {took:?}");
        // The queue holds four records while the writer is stuck: the rest are dropped.
        assert_eq!(dropped, 1000 - 4);
        assert!(!flushed_while_stuck, "nothing past the first record was written yet");
        assert!(flushed);
        assert_eq!(reported.load(Ordering::Relaxed), dropped, "the writer reports every drop once");
        let got = String::from_utf8(got.lock().unwrap().clone()).unwrap();
        assert_eq!(got.lines().count(), 1 + 4);
        assert!(got.starts_with("first\n"));
    }

    #[test]
    fn records_reach_the_sink_whole_and_in_order() {
        let (q, rx) = LogQueue::new(QUEUE_LINES);
        let got = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|s| {
            let sink = Sink { got: got.clone(), gate: None };
            let qd = q.clone();
            s.spawn(move || qd.drain(rx, sink, |_| panic!("nothing may be dropped")));
            let mut w = QueueWriter(q.clone());
            for i in 0..5000 {
                write!(w, "[line {i}] x\n").unwrap();
            }
            assert!(q.flush(Duration::from_secs(5)));
            assert_eq!(q.dropped(), 0);
            drop(w);
            q.close();
        });
        let got = String::from_utf8(got.lock().unwrap().clone()).unwrap();
        let want: String = (0..5000).map(|i| format!("[line {i}] x\n")).collect();
        assert_eq!(got, want);
    }
}
