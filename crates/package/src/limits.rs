//! Named size limits applied while building and reading package archives.
//!
//! The values bound the memory and disk an installer spends on an archive it
//! has not yet authenticated. They are generous for a package of manifests,
//! detection files and small integration assets, and far below anything that
//! could exhaust a developer machine.

// Rust guideline compliant 2026-10-04

/// Largest accepted compressed archive: 8 MiB.
///
/// Checked before any decompression. A package holds TOML manifests and small
/// integration assets, so real archives are tens of KiB; the ceiling leaves
/// room for bundled helper scripts. Raising it widens the amount of
/// unauthenticated data an installer buffers.
pub const MAX_COMPRESSED_BYTES: u64 = 8 * 1024 * 1024;

/// Largest accepted decompressed tar stream, headers and padding included:
/// 64 MiB.
///
/// Bounds the memory the reader allocates while inflating. It equals eight
/// times [`MAX_COMPRESSED_BYTES`], which a text-heavy package can legitimately
/// approach, and stays well under the [`MAX_EXPANSION_RATIO`] product for
/// maximum-size archives.
pub const MAX_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;

/// Largest accepted ratio between decompressed and compressed size: 100.
///
/// Catches decompression bombs early: a tiny archive that inflates to the
/// absolute ceiling is rejected as soon as the decoded output passes
/// `compressed_len * ratio`. Typical text compresses 3-10x and the 1 KiB tar
/// end marker of a one-file package compresses about 30x, so 100 leaves
/// headroom for legitimate archives while zero-filled bombs (1000x and more)
/// fail within a few KiB of output.
pub const MAX_EXPANSION_RATIO: u64 = 100;

/// Largest accepted number of files in one archive: 512.
///
/// A runtime package has a handful of manifests plus integration assets; this
/// bounds per-entry bookkeeping and the staging directory fan-out.
pub const MAX_FILES: usize = 512;

/// Largest accepted path length in bytes: 100.
///
/// This is the capacity of the USTAR `name` field. The format never uses the
/// USTAR `prefix` field, so a longer path cannot be represented. A [`Limits`]
/// value above this cap is treated as this cap.
pub const MAX_PATH_BYTES: usize = 100;

/// Largest accepted size of a single file: 16 MiB.
///
/// Larger than any manifest or asset a package should carry, and a quarter of
/// [`MAX_EXPANDED_BYTES`] so a single entry cannot consume the whole budget.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Largest accepted zstd back-reference window: 8 MiB.
///
/// The zstd format recommends 8 MiB as the interchange ceiling. The decoder
/// allocates a window of the size the frame header declares, so the check runs
/// on the header, before allocation. The builder's own frames use a much
/// smaller window.
pub const MAX_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// Resource limits for building and reading one archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Largest compressed archive in bytes.
    pub max_compressed_bytes: u64,
    /// Largest decompressed tar stream in bytes.
    pub max_expanded_bytes: u64,
    /// Largest decompressed-to-compressed size ratio.
    pub max_expansion_ratio: u64,
    /// Largest number of files.
    pub max_files: usize,
    /// Largest path length in bytes; capped at [`MAX_PATH_BYTES`].
    pub max_path_bytes: usize,
    /// Largest size of one file in bytes.
    pub max_file_bytes: u64,
    /// Largest accepted zstd window in bytes.
    pub max_window_bytes: u64,
}

impl Limits {
    /// The production limits named by the module-level constants.
    pub const DEFAULT: Self = Self {
        max_compressed_bytes: MAX_COMPRESSED_BYTES,
        max_expanded_bytes: MAX_EXPANDED_BYTES,
        max_expansion_ratio: MAX_EXPANSION_RATIO,
        max_files: MAX_FILES,
        max_path_bytes: MAX_PATH_BYTES,
        max_file_bytes: MAX_FILE_BYTES,
        max_window_bytes: MAX_WINDOW_BYTES,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
