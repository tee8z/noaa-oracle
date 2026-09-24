//! Keep slow stdout writes off request threads. Queue complete records with a
//! fixed byte limit; report saturation after the output resumes. The main
//! function owns the guard and drains it after application shutdown.

use fern::{Dispatch, Output};
use std::{
    fmt::{self, Write as _},
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

const CAPACITY: usize = 256;
const RECORD_BYTES: usize = 16 * 1024;
const TRUNCATED: &str = " [truncated]\n";
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

struct Queue {
    // Fern's global callback only retains this Arc, never a cloned Sender.
    // Taking this one sender closes the queue even while the logger lives.
    sender: Mutex<Option<SyncSender<String>>>,
    dropped: AtomicU64,
    pending: AtomicU64,
}

impl Queue {
    fn drop_records(&self, count: u64) {
        self.dropped.fetch_add(count, Ordering::Relaxed);
        self.pending.fetch_add(count, Ordering::Relaxed);
    }

    fn send(&self, record: String) {
        let sender = self.sender.lock().unwrap_or_else(|p| p.into_inner());
        if sender
            .as_ref()
            .is_none_or(|sender| sender.try_send(record).is_err())
        {
            self.drop_records(1);
        }
    }
}

/// Closes the logger queue and allows at most two seconds for stdout to drain.
/// If stdout remains blocked, dropping the guard does not join its writer.
/// Records still waiting at process exit can be lost.
pub struct LogGuard {
    queue: Arc<Queue>,
    finished: Option<Receiver<io::Result<()>>>,
}

impl LogGuard {
    /// Records rejected by a saturated/closed queue or a failed output writer.
    pub fn dropped_messages(&self) -> u64 {
        self.queue.dropped.load(Ordering::Relaxed)
    }

    fn drain(&mut self, timeout: Duration) -> io::Result<bool> {
        self.queue
            .sender
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let Some(finished) = &self.finished else {
            return Ok(true);
        };
        match finished.recv_timeout(timeout) {
            Ok(result) => {
                self.finished = None;
                result.map(|()| true)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.finished = None;
                Err(io::Error::other(
                    "log writer stopped without reporting completion",
                ))
            }
        }
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let _ = self.drain(DRAIN_TIMEOUT);
    }
}

struct Record(String, bool);
impl fmt::Write for Record {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.1 {
            return Ok(());
        }
        let remaining = (RECORD_BYTES - TRUNCATED.len()).saturating_sub(self.0.len());
        let end = value.floor_char_boundary(remaining.min(value.len()));
        self.0.push_str(&value[..end]);
        self.1 |= end < value.len();
        Ok(())
    }
}

fn format_record(args: fmt::Arguments<'_>) -> String {
    let mut record = Record(String::with_capacity(512), false);
    let _ = record.write_fmt(args);
    if record.1 {
        record.0.push_str(TRUNCATED);
    }
    record.0
}

fn report_drops(writer: &mut impl Write, queue: &Queue) -> io::Result<()> {
    let count = queue.pending.swap(0, Ordering::Relaxed);
    if count != 0 {
        writeln!(
            writer,
            "[oracle logging] dropped {count} records while the output queue was unavailable"
        )?;
    }
    Ok(())
}

fn buffered(
    writer: impl Write + Send + 'static,
    capacity: usize,
) -> io::Result<(Output, LogGuard)> {
    let (sender, receiver) = mpsc::sync_channel::<String>(capacity);
    let queue = Arc::new(Queue {
        sender: Mutex::new(Some(sender)),
        dropped: AtomicU64::new(0),
        pending: AtomicU64::new(0),
    });
    let (complete, finished) = mpsc::sync_channel(1);
    let worker_queue = queue.clone();
    std::thread::Builder::new()
        .name("oracle-log-writer".into())
        .spawn(move || {
            let mut writer = writer;
            let result = (|| {
                while let Ok(record) = receiver.recv() {
                    if let Err(error) = report_drops(&mut writer, &worker_queue)
                        .and_then(|()| writer.write_all(record.as_bytes()))
                        .and_then(|()| writer.flush())
                    {
                        worker_queue
                            .sender
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .take();
                        worker_queue.drop_records(1 + receiver.try_iter().count() as u64);
                        return Err(error);
                    }
                }
                report_drops(&mut writer, &worker_queue)?;
                writer.flush()
            })();
            let _ = complete.send(result);
        })?;
    let output_queue = queue.clone();
    // Output::call receives one formatted record. Its flush is a no-op, so
    // neither enqueue nor log::logger().flush() waits for stdout.
    let output = Output::call(move |record| {
        output_queue.send(format_record(format_args!("{}\n", record.args())));
    });
    Ok((
        output,
        LogGuard {
            queue,
            finished: Some(finished),
        },
    ))
}

/// Normal formatted logging, buffered by at most 256 records of 16 KiB each.
/// Keep the returned guard alive until application work has stopped.
pub fn setup_buffered_logger() -> io::Result<(Dispatch, LogGuard)> {
    let (output, guard) = buffered(io::stdout(), CAPACITY)?;
    Ok((crate::config::logger_to(output), guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct BlockedWriter {
        output: Arc<Mutex<Vec<u8>>>,
        started: Option<SyncSender<()>>,
        release: Receiver<()>,
        flushed: Arc<AtomicU64>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
                self.release
                    .recv()
                    .map_err(|_| io::Error::other("test writer released by disconnect"))?;
            }
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn blocked_output_keeps_whole_records_nonblocking_and_drains_after_global_logger_closes() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let flushed = Arc::new(AtomicU64::new(0));
        let (started, waiting) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let writer = BlockedWriter {
            output: output.clone(),
            started: Some(started),
            release: released,
            flushed: flushed.clone(),
        };
        let (sink, mut guard) = buffered(writer, 2).unwrap();
        let (_, logger) = Dispatch::new().chain(sink).into_log();
        let send = |number| {
            logger.log(
                &log::Record::builder()
                    .level(log::Level::Info)
                    .args(format_args!("record {number}"))
                    .build(),
            )
        };
        send(1);
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        let began = Instant::now();
        send(2);
        send(3);
        send(4);
        logger.flush();
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "producer waited for blocked stdout"
        );
        assert_eq!(guard.dropped_messages(), 1);
        let began = Instant::now();
        assert!(!guard.drain(Duration::from_millis(10)).unwrap());
        assert!(
            began.elapsed() < Duration::from_millis(200),
            "shutdown joined blocked writer"
        );
        // The globally retained logger stays alive. Explicit close still lets
        // the worker finish after stdout resumes, with accepted records intact.
        release.send(()).unwrap();
        assert!(guard.drain(Duration::from_secs(1)).unwrap());
        let bytes = output.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("record 1\n")
                && text.contains("record 2\n")
                && text.contains("record 3\n")
        );
        assert!(!text.contains("record 4\n"));
        assert!(text.contains("dropped 1 records"));
        assert!(flushed.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn formatted_record_has_a_byte_bound_without_splitting_utf8() {
        let message = "é".repeat(RECORD_BYTES);
        let record = format_record(format_args!("prefix {message} suffix\n"));
        assert!(record.len() <= RECORD_BYTES);
        assert!(record.starts_with("prefix é"));
        assert!(record.ends_with(TRUNCATED));
    }

    #[test]
    fn writer_failure_does_not_panic_or_block_the_producer() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (sink, mut guard) = buffered(Broken, 2).unwrap();
        let (_, logger) = Dispatch::new().chain(sink).into_log();
        logger.log(
            &log::Record::builder()
                .args(format_args!("lost record"))
                .build(),
        );
        assert!(guard.drain(Duration::from_secs(1)).is_err());
        logger.log(
            &log::Record::builder()
                .args(format_args!("closed writer"))
                .build(),
        );
        assert_eq!(guard.dropped_messages(), 2);
    }
}
