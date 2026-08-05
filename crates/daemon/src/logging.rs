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

use pohunek_logging::Writer;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use crate::error::DaemonError;

/// Default log filter when `RUST_LOG` is not set.
const DEFAULT_FILTER: &str = "info";
/// Guard that must be kept alive for non-blocking log flushing.
///
/// Dropping it flushes buffered log lines. `main` holds it for the process
/// lifetime.
pub type LogGuard = tracing_appender::non_blocking::WorkerGuard;

/// Initialize JSON logging to `log_dir`.
///
/// Returns a guard the caller must keep alive. The filter honors `RUST_LOG`,
/// defaulting to `info`.
///
/// # Errors
///
/// Returns a typed [`DaemonError`] when the bounded writer or global subscriber
/// cannot be initialized.
pub fn init(log_dir: &Path) -> Result<LogGuard, DaemonError> {
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

    tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_env_filter(filter)
        .with_writer(non_blocking.and(stderr))
        .try_init()
        .map_err(|error| DaemonError::LoggingSubscriber(error.to_string()))?;

    Ok(guard)
}
