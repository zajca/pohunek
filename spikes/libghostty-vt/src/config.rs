//! Bounds and settings read from `config/model.toml`.
//!
//! Every key is required. A missing or unknown key is a typed error, never a
//! silent default.

// Rust guideline compliant 2026-06-26

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::trace::Limits;

/// The parsed `config/model.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// Largest trace file the parser reads.
    pub max_trace_bytes: usize,
    /// Largest `Output` record a trace may hold.
    pub max_record_bytes: usize,
    /// Most records one trace may hold.
    pub max_records: usize,
    /// Smallest row or column count.
    pub min_dimension: u16,
    /// Largest row count.
    pub max_rows: u16,
    /// Largest column count.
    pub max_cols: u16,
    /// Bytes one recorder PTY read asks for.
    pub read_chunk_bytes: usize,
    /// Wall-clock limit of one recording.
    pub record_timeout_seconds: u64,
    /// Marker a recorded script prints before it waits for a line.
    pub resize_marker: String,
    /// `TERM` of a recorded child.
    pub child_term: String,
    /// `LANG` of a recorded child.
    pub child_lang: String,
    /// `PATH` of a recorded child.
    pub child_path: String,
    /// libghostty scrollback limit in bytes.
    pub ghostty_scrollback_max_bytes: usize,
    /// libghostty largest APC sequence in bytes.
    pub ghostty_apc_max_bytes: usize,
    /// libghostty largest kept unknown sequence in bytes.
    pub ghostty_unknown_max_bytes: usize,
    /// libghostty largest parser continuation in bytes.
    pub ghostty_continuation_max_bytes: usize,
    /// libghostty largest OSC 5522 write transaction in bytes.
    pub ghostty_clipboard_write_max_bytes: usize,
    /// Cell size in pixels passed to libghostty on resize.
    pub ghostty_cell_px: u32,
    /// Scrollback rows of the `vt100-scrollback` model.
    pub vt100_scrollback_rows: usize,
    /// Fixture the throughput measurement uses as the agent trace.
    pub throughput_agent_trace: String,
    /// Times the agent trace is replayed in one throughput run.
    pub throughput_agent_replays: usize,
    /// Size of the synthetic flood of one throughput run.
    pub throughput_flood_bytes: usize,
    /// Chunk size the flood is fed in, like a PTY read.
    pub throughput_flood_chunk_bytes: usize,
    /// Seed of the flood generator.
    pub throughput_flood_seed: u64,
    /// Runs per model and workload; the report keeps the median.
    pub throughput_runs: usize,
    /// Prefixes a neutral assertion source starts with.
    pub allowed_source_prefixes: Vec<String>,
    /// Words an assertion source must not contain (case-insensitive).
    pub forbidden_source_words: Vec<String>,
    /// Repository path prefixes a `pohunek-contract` source starts with.
    pub contract_source_prefixes: Vec<String>,
}

/// Why `config/model.toml` could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O failure.
        source: std::io::Error,
    },
    /// The text is not valid for [`ModelConfig`].
    #[error("invalid config {path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message, with line and column.
        message: String,
    },
    /// A bound is inconsistent.
    #[error("invalid config {path}: {reason}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is inconsistent.
        reason: &'static str,
    },
}

impl ModelConfig {
    /// Reads and validates the config file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read, a key is missing
    /// or unknown, or the bounds contradict each other.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    /// Parses `text` as the config file `path`.
    ///
    /// # Errors
    ///
    /// Same as [`ModelConfig::load`], without the read failure.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let invalid = |reason| ConfigError::Invalid {
            path: path.to_path_buf(),
            reason,
        };
        if config.min_dimension == 0 || config.min_dimension > config.max_rows.min(config.max_cols)
        {
            return Err(invalid("min_dimension must be 1..=min(max_rows, max_cols)"));
        }
        if config.read_chunk_bytes == 0 || config.read_chunk_bytes > config.max_record_bytes {
            return Err(invalid("read_chunk_bytes must be 1..=max_record_bytes"));
        }
        if config.throughput_runs == 0
            || config.throughput_agent_replays == 0
            || config.throughput_flood_bytes == 0
            || config.throughput_flood_chunk_bytes == 0
            || config.vt100_scrollback_rows == 0
            || config.throughput_flood_seed == 0
        {
            return Err(invalid(
                "the throughput runs, replays, flood size, flood chunk and seed and the vt100 scrollback rows must be positive",
            ));
        }
        if config.resize_marker.is_empty() {
            return Err(invalid("resize_marker must not be empty"));
        }
        Ok(config)
    }

    /// The trace parser bounds.
    #[must_use]
    pub fn limits(&self) -> Limits {
        Limits {
            max_trace_bytes: self.max_trace_bytes,
            max_record_bytes: self.max_record_bytes,
            max_records: self.max_records,
            min_dimension: self.min_dimension,
            max_rows: self.max_rows,
            max_cols: self.max_cols,
        }
    }
}
