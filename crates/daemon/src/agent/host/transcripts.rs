//! A runtime's transcript declaration: where below its config home the agent
//! writes conversation transcripts, and which compiled parser reads them.
//!
//! A descriptor declares it in an optional `[transcripts]` table next to the
//! `[config_home]` table it is relative to:
//!
//! ```toml
//! [transcripts]
//! parser = "jsonl-head-v1"
//! dir = "sessions"
//! ```
//!
//! The parser is a closed set compiled into core; a descriptor only names one.
//! A runtime without the table contributes no transcript root to the external
//! observer.

// Rust guideline compliant 2026-10-12

use super::definition::DefinitionError;
use crate::agent::{is_plain_relative_path, MAX_EXISTENCE_PATH_COMPONENTS};

/// Id of the parser that reads session identity and working directory from
/// the first JSON Lines records of a transcript.
pub const JSONL_HEAD_V1_PARSER_ID: &str = "jsonl-head-v1";

/// Longest declared transcript directory, in bytes.
///
/// The directory is joined below a config home and walked by the observer on
/// every pass, so it is a short descriptor-relative path, never a location.
/// It shares the bound of every other descriptor-relative path
/// (`config_home.default`, `native_reference.existence.dir`) so one rule
/// governs them all; raising it only admits longer paths, not new ones.
pub const MAX_TRANSCRIPT_DIR_BYTES: usize = crate::agent::MAX_EXISTENCE_TEXT_BYTES;

/// Most components a declared transcript directory may have.
///
/// Shares the descriptor-relative path depth bound for the same reason as
/// [`MAX_TRANSCRIPT_DIR_BYTES`].
pub const MAX_TRANSCRIPT_DIR_COMPONENTS: usize = MAX_EXISTENCE_PATH_COMPONENTS;

/// A transcript parser compiled into core.
///
/// The set is closed: a descriptor that names any other id is refused when it
/// is parsed, so an installed runtime never carries a parser core lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptParser {
    /// Reads at most a bounded head of a JSON Lines transcript and takes the
    /// first `session_id`, `sessionId`, `conversation_id` or `conversationId`
    /// string (in that order within one record), the first `transcript_path`
    /// or `transcriptPath`, and the first absolute `cwd`, `working_directory`
    /// or `workingDirectory`.
    JsonlHeadV1,
}

impl TranscriptParser {
    /// The compiled parser named `id`, if core provides one.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            JSONL_HEAD_V1_PARSER_ID => Some(Self::JsonlHeadV1),
            _ => None,
        }
    }

    /// The id a descriptor names this parser by.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::JsonlHeadV1 => JSONL_HEAD_V1_PARSER_ID,
        }
    }
}

/// A validated `[transcripts]` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcripts {
    parser: TranscriptParser,
    dir: String,
}

impl Transcripts {
    /// Resolves `parser` against the compiled set and validates `dir`.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when `parser` is not a compiled
    /// transcript parser, or `dir` is not a relative path of plain components
    /// (no absolute path, no `.` or `..`, no empty segment, no glob or shell
    /// metacharacter) within [`MAX_TRANSCRIPT_DIR_BYTES`] and
    /// [`MAX_TRANSCRIPT_DIR_COMPONENTS`].
    pub fn new(parser: &str, dir: &str) -> Result<Self, DefinitionError> {
        let parser = TranscriptParser::from_id(parser).ok_or(DefinitionError::Field {
            field: "transcripts.parser",
            reason: "is not a transcript parser this build provides",
        })?;
        let bounded = dir.len() <= MAX_TRANSCRIPT_DIR_BYTES
            && dir.split('/').count() <= MAX_TRANSCRIPT_DIR_COMPONENTS;
        if !bounded || !is_plain_relative_path(dir) {
            return Err(DefinitionError::Field {
                field: "transcripts.dir",
                reason: "must be a bounded relative path of plain components",
            });
        }
        Ok(Self {
            parser,
            dir: dir.to_owned(),
        })
    }

    /// The compiled parser that reads the runtime's transcripts.
    #[must_use]
    pub fn parser(&self) -> TranscriptParser {
        self.parser
    }

    /// The transcript tree below the config home, as declared.
    #[must_use]
    pub fn dir(&self) -> &str {
        &self.dir
    }
}
