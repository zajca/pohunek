//! Structured logging setup.
//!
//! Emits JSON `tracing` logs to a daily-rotated file under the daemon's log
//! directory (`~/.local/state/pohunek/logs/`), per `docs/architecture.md`
//! "Logging and Observability". The log directory is created on startup.
//!
//! Terminal content and secrets are never logged by the daemon's own spans; that
//! discipline is the caller's responsibility at each log site (the plan forbids
//! logging raw terminal streams).

use std::path::Path;

use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use crate::error::DaemonError;

/// Default log filter when `RUST_LOG` is not set.
const DEFAULT_FILTER: &str = "info";
/// Log filename prefix for the rolling file appender.
const LOG_PREFIX: &str = "pohunekd.log";

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
/// Returns [`DaemonError::Directory`] if the log directory cannot be created.
pub fn init(log_dir: &Path) -> Result<LogGuard, DaemonError> {
    std::fs::create_dir_all(log_dir).map_err(|source| DaemonError::Directory {
        path: log_dir.to_path_buf(),
        source,
    })?;

    let file_appender = tracing_appender::rolling::daily(log_dir, LOG_PREFIX);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

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
        .init();

    Ok(guard)
}
