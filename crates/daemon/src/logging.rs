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

use std::path::Path;
use std::sync::OnceLock;

use pohunek_logging::Writer;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::writer::MakeWriterExt;
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

/// Guards that keep a [`scoped`] logging sink installed.
///
/// Dropping it flushes the buffered log lines and restores the previously
/// installed subscriber of the thread.
#[derive(Debug)]
#[must_use = "dropping the guards ends the scoped logging sink"]
pub struct ScopedLogGuard {
    // Field order is drop order: the subscriber is uninstalled before the writer
    // flushes.
    _default: DefaultGuard,
    _flush: LogGuard,
}

/// Install the production JSON logging sink for the current thread only.
///
/// The sink is built like the one [`init`] installs, but it replaces no
/// process-wide state, so several callers in one process (tests sharing a
/// binary) each capture their own log directory. [`init`] stays the only entry
/// point that installs a process-wide subscriber and still fails when one is
/// already set.
///
/// Only events emitted on the calling thread reach the sink; events from other
/// threads (blocking-pool closures, the event-journal writer thread) are not
/// captured. Use it on a current-thread runtime, where every `.await` resumes on
/// the installing thread.
///
/// A process-wide `NoSubscriber` anchor is registered first. With a single
/// registered dispatcher `tracing` computes a callsite's interest from the
/// first thread that reaches it, and a thread without a default would cache
/// "never" for every other thread. With the anchor present, interest is
/// combined over all dispatchers and each event consults its own thread's
/// default.
///
/// # Errors
///
/// Returns a typed [`DaemonError`] when the bounded writer cannot be opened.
pub fn scoped(log_dir: &Path) -> Result<ScopedLogGuard, DaemonError> {
    static ANCHOR: OnceLock<tracing::Dispatch> = OnceLock::new();
    ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
    let (subscriber, flush) = subscriber(log_dir)?;
    Ok(ScopedLogGuard {
        _default: subscriber.set_default(),
        _flush: flush,
    })
}

/// Build the JSON subscriber writing to `log_dir`, and the guard that flushes it.
fn subscriber(
    log_dir: &Path,
) -> Result<(impl tracing::Subscriber + Send + Sync + 'static, LogGuard), DaemonError> {
    let writer = Writer::open(
        log_dir,
        pohunek_logging::config::daemon_files()?,
        pohunek_logging::config::daemon_policy()?,
    )?;
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    // Also mirror to stderr so foreground `daemon start` shows activity; the
    // file is the durable JSON record for backtesting.
    let stderr = std::io::stderr.with_max_level(tracing::Level::INFO);

    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_env_filter(filter)
        .with_writer(non_blocking.and(stderr))
        .finish();
    Ok((subscriber, guard))
}
