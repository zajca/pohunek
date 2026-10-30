//! Structured logging setup.
//!
//! Emits bounded JSON `tracing` logs under the daemon's log directory
//! (`~/.local/state/pohunek/logs/`), per `docs/architecture.md` "Logging and
//! Observability". The log directory is created on startup.
//!
//! Terminal content and secrets are never logged by the daemon's own spans; that
//! discipline is the caller's responsibility at each log site (the plan forbids
//! logging raw terminal streams).

// Rust guideline compliant 2026-07-28

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use pohunek_logging::Writer;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::error::DaemonError;

/// Default log filter when `RUST_LOG` is not set.
const DEFAULT_FILTER: &str = "info";
/// Guard that must be kept alive for non-blocking log flushing.
///
/// Dropping it flushes buffered log lines. `main` holds it for the process
/// lifetime.
pub type LogGuard = tracing_appender::non_blocking::WorkerGuard;

/// Initialize JSON logging to `log_dir` for the whole process.
///
/// Returns a guard the caller must keep alive. The filter honors `RUST_LOG`,
/// defaulting to `info`.
///
/// # Errors
///
/// Returns a typed [`DaemonError`] when the bounded writer or global subscriber
/// cannot be initialized.
pub fn init(log_dir: &Path) -> Result<LogGuard, DaemonError> {
    let (subscriber, guard) = subscriber(log_dir)?;
    subscriber
        .try_init()
        .map_err(|error| DaemonError::LoggingSubscriber(error.to_string()))?;
    Ok(guard)
}

/// Guard of a [`capture`]: while it lives, the daemon's JSON log of every thread
/// of the process is written to the capture's directory.
///
/// Dropping it ends the capture; every event is on disk by then, because the
/// capture writes synchronously.
#[derive(Debug)]
#[must_use = "dropping the guard ends the capture"]
pub struct CaptureGuard {
    // Released after the sink is detached: the next capture starts only once
    // this one has ended.
    _serial: MutexGuard<'static, ()>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        // Detach the sink first so a late event of another thread is discarded.
        *lock_ignoring_poison(&CAPTURE_SINK) = None;
    }
}

/// Capture the production JSON log of the whole process in `log_dir`.
///
/// The sink is the bounded daemon log writer [`init`] installs, written to
/// synchronously, so no event is lost to a full channel or an unflushed buffer.
/// The events of every thread reach it: blocking-pool closures and the
/// event-journal writer thread, not only the calling one, and so do the events
/// of other tests running in the process. The first capture of a process
/// installs one process-wide subscriber whose file writer forwards to the
/// capture that is active and discards while none is (the stderr mirror keeps
/// printing). A capture holds a process-wide lock, so tests of one binary that
/// run on several threads capture one after the other, each into its own
/// directory; a second capture on a thread that holds one blocks forever.
/// [`init`] stays the entry point of the daemon and still fails when a
/// subscriber is already set, a capture's included.
///
/// # Errors
///
/// Returns a typed [`DaemonError`] when the bounded writer cannot be opened or
/// another process-wide subscriber is already installed.
pub fn capture(log_dir: &Path) -> Result<CaptureGuard, DaemonError> {
    let serial = lock_ignoring_poison(&CAPTURE_SERIAL);
    install_capture_subscriber()?;
    let writer = open_writer(log_dir)?;
    *lock_ignoring_poison(&CAPTURE_SINK) = Some(writer);
    Ok(CaptureGuard { _serial: serial })
}

/// Serializes captures.
static CAPTURE_SERIAL: Mutex<()> = Mutex::new(());
/// The writer of the active capture.
static CAPTURE_SINK: Mutex<Option<Writer>> = Mutex::new(None);
/// Outcome of installing the process-wide capture subscriber.
static CAPTURE_SUBSCRIBER: OnceLock<Result<(), String>> = OnceLock::new();

fn lock_ignoring_poison<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn install_capture_subscriber() -> Result<(), DaemonError> {
    CAPTURE_SUBSCRIBER
        .get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .with_env_filter(filter())
                .with_writer(CaptureWriters.and(stderr_mirror()))
                .finish();
            // `try_init` also installs the `log` bridge, like [`init`], so a
            // `log`-crate record of a dependency reaches the capture.
            subscriber.try_init().map_err(|error| error.to_string())
        })
        .clone()
        .map_err(DaemonError::LoggingSubscriber)
}

/// Makes the writers of the process-wide capture subscriber.
struct CaptureWriters;

impl<'a> MakeWriter<'a> for CaptureWriters {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> CaptureWriter {
        CaptureWriter
    }
}

/// Forwards to the writer of the active capture, and discards when none is.
struct CaptureWriter;

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match lock_ignoring_poison(&CAPTURE_SINK).as_mut() {
            Some(sink) => sink.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match lock_ignoring_poison(&CAPTURE_SINK).as_mut() {
            Some(sink) => sink.flush(),
            None => Ok(()),
        }
    }
}

/// Build the JSON subscriber writing to `log_dir`, and the guard that flushes it.
fn subscriber(
    log_dir: &Path,
) -> Result<(impl tracing::Subscriber + Send + Sync + 'static, LogGuard), DaemonError> {
    let (non_blocking, guard) = tracing_appender::non_blocking(open_writer(log_dir)?);
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_env_filter(filter())
        // Also mirror to stderr so foreground `daemon start` shows activity; the
        // file is the durable JSON record for backtesting.
        .with_writer(non_blocking.and(stderr_mirror()))
        .finish();
    Ok((subscriber, guard))
}

/// The bounded daemon log writer on `log_dir`.
fn open_writer(log_dir: &Path) -> Result<Writer, DaemonError> {
    Ok(Writer::open(
        log_dir,
        pohunek_logging::config::daemon_files()?,
        pohunek_logging::config::daemon_policy()?,
    )?)
}

/// The log filter: `RUST_LOG`, or [`DEFAULT_FILTER`] when it is not set.
fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// The stderr mirror of the file log, at `info` and above.
fn stderr_mirror() -> impl for<'a> MakeWriter<'a> + 'static {
    std::io::stderr.with_max_level(tracing::Level::INFO)
}
