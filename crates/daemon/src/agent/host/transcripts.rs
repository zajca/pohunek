//! A runtime's transcript declaration: where the agent writes conversation
//! transcripts, and which compiled parser reads them.
//!
//! A descriptor declares it in an optional `[transcripts]` table. By default
//! `dir` is relative to the `[config_home]` table next to it:
//!
//! ```toml
//! [transcripts]
//! parser = "jsonl-head-v1"
//! dir = "sessions"
//! ```
//!
//! An agent that keeps its transcripts outside the config home (under
//! `XDG_DATA_HOME`, say) names the root itself with `root_env` and `root_home`,
//! which resolve like a `[config_home]` (`env` and `default`), and bounds the
//! walk with `max_depth`, the directory levels below `dir` the observer lists
//! (feature `transcripts-data-root-v1`):
//!
//! ```toml
//! [transcripts]
//! parser = "muse-session-v1"
//! root_env = "XDG_DATA_HOME"
//! root_home = ".local/share"
//! dir = "muse/sessions"
//! max_depth = 4
//! ```
//!
//! `jsonl-head-v1` also reads a session id and a working directory that sit
//! below an object key of the first record, when the descriptor names the key
//! paths (feature `transcripts-head-paths-v1`):
//!
//! ```toml
//! [transcripts]
//! parser = "jsonl-head-v1"
//! dir = "sessions"
//! id_path = ["payload", "id"]
//! cwd_path = ["payload", "cwd"]
//! ```
//!
//! The parser is a closed set compiled into core; a descriptor only names one.
//! A runtime without the table contributes no transcript root to the external
//! observer.

// Rust guideline compliant 2026-10-12

use super::config_home::ConfigHome;
use super::definition::DefinitionError;
use crate::agent::{is_plain_relative_path, MAX_EXISTENCE_PATH_COMPONENTS};

/// Id of the parser that reads session identity and working directory from
/// the first JSON Lines records of a transcript.
pub const JSONL_HEAD_V1_PARSER_ID: &str = "jsonl-head-v1";

/// Id of the parser that reads the session identity and working directory of
/// a Muse Code session log, `<root>/YYYY/MM/DD/<uuid>/session.jsonl`.
pub const MUSE_SESSION_PARSER_ID: &str = "muse-session-v1";

/// Most directory levels a `[transcripts] max_depth` may descend.
pub const MAX_TRANSCRIPT_DEPTH: u8 = 6;

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

/// Most object keys a `[transcripts]` head path may chain.
pub const MAX_HEAD_PATH_KEYS: usize = 3;

/// Longest key of a head path, in bytes.
///
/// Keys name fields of a transcript's first record, so they are short
/// identifiers; the bound keeps a descriptor from carrying free text.
pub const MAX_HEAD_PATH_KEY_BYTES: usize = 32;

/// A validated chain of object keys that leads to a string in the first record
/// of a transcript.
///
/// A key is `[A-Za-z0-9_]`, so a path is data to walk, never an expression: no
/// separator, index, wildcard or escape exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadKeyPath(Vec<String>);

impl HeadKeyPath {
    /// Validates `keys` as the path declared at `field`.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when the path is empty or longer
    /// than [`MAX_HEAD_PATH_KEYS`], or a key is empty, longer than
    /// [`MAX_HEAD_PATH_KEY_BYTES`] or holds a character outside
    /// `[A-Za-z0-9_]`.
    pub fn new(field: &'static str, keys: &[String]) -> Result<Self, DefinitionError> {
        let key_is_plain = |key: &String| {
            !key.is_empty()
                && key.len() <= MAX_HEAD_PATH_KEY_BYTES
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        };
        if keys.is_empty() || keys.len() > MAX_HEAD_PATH_KEYS || !keys.iter().all(key_is_plain) {
            return Err(DefinitionError::Field {
                field,
                reason: "must be a short list of plain object keys",
            });
        }
        Ok(Self(keys.to_vec()))
    }

    /// The object keys, outermost first.
    #[must_use]
    pub fn keys(&self) -> &[String] {
        &self.0
    }
}

/// The key paths a `jsonl-head-v1` declaration names for the first record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadPaths {
    id: Option<HeadKeyPath>,
    cwd: Option<HeadKeyPath>,
}

impl HeadPaths {
    /// The path of the native session id, when declared.
    #[must_use]
    pub fn id(&self) -> Option<&HeadKeyPath> {
        self.id.as_ref()
    }

    /// The path of the working directory, when declared.
    #[must_use]
    pub fn cwd(&self) -> Option<&HeadKeyPath> {
        self.cwd.as_ref()
    }
}

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
    /// Reads a session log whose lines are framed transactions
    /// (`children[].record_json`) or bare records, within the same bounded
    /// head, and takes the first `runtime.session.metadata` record: the
    /// session id from `stream.id`, which must equal the `<uuid>` directory
    /// name, and the absolute working directory from
    /// `payload.record.workspace_root`. Only a log at exactly
    /// `YYYY/MM/DD/<uuid>/session.jsonl` below the root is a session.
    MuseSessionV1,
}

impl TranscriptParser {
    /// The compiled parser named `id`, if core provides one.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            JSONL_HEAD_V1_PARSER_ID => Some(Self::JsonlHeadV1),
            MUSE_SESSION_PARSER_ID => Some(Self::MuseSessionV1),
            _ => None,
        }
    }

    /// The id a descriptor names this parser by.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::JsonlHeadV1 => JSONL_HEAD_V1_PARSER_ID,
            Self::MuseSessionV1 => MUSE_SESSION_PARSER_ID,
        }
    }

    /// The depth of the session directories below the root when the tree is
    /// sharded by date, so a walk that ends at a bound must have visited the
    /// newest directories first and only the newest session directories hold
    /// a watch.
    #[must_use]
    pub fn dated_depth(self) -> Option<u8> {
        match self {
            Self::MuseSessionV1 => Some(4),
            Self::JsonlHeadV1 => None,
        }
    }
}

/// A validated `[transcripts]` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcripts {
    parser: TranscriptParser,
    dir: String,
    root: Option<ConfigHome>,
    max_depth: Option<u8>,
    head_paths: HeadPaths,
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
            root: None,
            max_depth: None,
            head_paths: HeadPaths::default(),
        })
    }

    /// This declaration reading the session id and the working directory from
    /// the declared key paths of the first record.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when the parser is not
    /// `jsonl-head-v1`, a path is not a short list of plain object keys, or
    /// neither path is given.
    pub fn with_head_paths(
        mut self,
        id_path: Option<&[String]>,
        cwd_path: Option<&[String]>,
    ) -> Result<Self, DefinitionError> {
        if self.parser != TranscriptParser::JsonlHeadV1 {
            return Err(DefinitionError::Field {
                field: "transcripts.parser",
                reason: "must be jsonl-head-v1 to declare head paths",
            });
        }
        self.head_paths = HeadPaths {
            id: id_path
                .map(|keys| HeadKeyPath::new("transcripts.id_path", keys))
                .transpose()?,
            cwd: cwd_path
                .map(|keys| HeadKeyPath::new("transcripts.cwd_path", keys))
                .transpose()?,
        };
        Ok(self)
    }

    /// The key paths of the first record; empty unless declared.
    #[must_use]
    pub fn head_paths(&self) -> &HeadPaths {
        &self.head_paths
    }

    /// This declaration with its own root: `root_env` names the variable that
    /// relocates the root and `root_home` is the directory below the user's
    /// home directory that applies while the variable is unset or empty.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming `transcripts.root_env` or
    /// `transcripts.root_home` when they do not validate like the `env` and
    /// `default` of a `[config_home]`.
    pub fn with_root(mut self, root_env: &str, root_home: &str) -> Result<Self, DefinitionError> {
        let root = ConfigHome::new(root_env, root_home).map_err(|error| match error {
            DefinitionError::Field { field, reason } => DefinitionError::Field {
                field: if field == "config_home.env" {
                    "transcripts.root_env"
                } else {
                    "transcripts.root_home"
                },
                reason,
            },
            other => other,
        })?;
        self.root = Some(root);
        Ok(self)
    }

    /// This declaration listing at most `max_depth` directory levels below
    /// `dir`.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when `max_depth` is zero or above
    /// [`MAX_TRANSCRIPT_DEPTH`].
    pub fn with_max_depth(mut self, max_depth: u64) -> Result<Self, DefinitionError> {
        match u8::try_from(max_depth) {
            Ok(depth) if (1..=MAX_TRANSCRIPT_DEPTH).contains(&depth) => {
                self.max_depth = Some(depth);
                Ok(self)
            }
            _ => Err(DefinitionError::Field {
                field: "transcripts.max_depth",
                reason: "must be between 1 and the supported transcript depth",
            }),
        }
    }

    /// The compiled parser that reads the runtime's transcripts.
    #[must_use]
    pub fn parser(&self) -> TranscriptParser {
        self.parser
    }

    /// The transcript tree below the root, as declared.
    #[must_use]
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// The root the descriptor names for itself, when the transcripts do not
    /// live below the config home.
    #[must_use]
    pub fn root(&self) -> Option<&ConfigHome> {
        self.root.as_ref()
    }

    /// The directory levels below [`Self::dir`] the observer lists; `None`
    /// bounds nothing.
    #[must_use]
    pub fn max_depth(&self) -> Option<u8> {
        self.max_depth
    }
}
