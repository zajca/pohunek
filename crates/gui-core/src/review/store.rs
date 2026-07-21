//! Persisted JSON review store: one file per review, atomic write-then-rename.
//!
//! The atomic pattern here (temp file + owner-private permissions + rename)
//! mirrors `crates/daemon/src/notifications/store.rs::write_policy`. The
//! reviews directory helper mirrors `crates/gui-core/src/ui_state.rs`'s
//! `default_state_dir`'s shape but reads `XDG_DATA_HOME`/`~/.local/share`
//! instead of `XDG_STATE_HOME`/`~/.local/state`; the two are deliberately
//! separate small implementations rather than a shared one, since
//! `pohunek-daemon` (owner of the notification store) is not a `gui-core`
//! dependency and `pohunek-paths`'s `APP_DIR` is `"pohunek"`, the daemon/CLI's
//! own data directory, not this GUI's.

// Rust guideline compliant 2026-07-19

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::model::{Review, ReviewId};

/// GUI application directory name under `XDG_DATA_HOME`/`~/.local/share`.
///
/// Same app-dir name as `ui_state`'s `XDG_STATE_HOME`-based state directory,
/// but a different XDG base (data, not state) — see the module doc comment.
const REVIEWS_APP_DIR: &str = "pohunek-gui";
/// Subdirectory holding one JSON file per review.
const REVIEWS_SUBDIR: &str = "reviews";

/// Unix file mode for a review JSON file: owner read/write only.
#[cfg(unix)]
const OWNER_PRIVATE_FILE_MODE: u32 = 0o600;

/// Errors raised while loading or saving review drafts.
#[derive(Debug, Error)]
pub enum ReviewStoreError {
    #[error(transparent)]
    MissingEnv(#[from] pohunek_paths::PathError),
    #[error("failed to create reviews directory `{}`: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write review `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to rename review `{}` into place: {source}", path.display())]
    Rename {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize review `{id}`: {source}")]
    Serialize {
        id: ReviewId,
        source: serde_json::Error,
    },
}

/// One review file's load failure, surfaced instead of skipped or panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLoadError {
    pub path: PathBuf,
    pub message: String,
}

/// Resolves the default reviews directory:
/// `$XDG_DATA_HOME/pohunek-gui/reviews`, falling back to
/// `~/.local/share/pohunek-gui/reviews`.
///
/// # Errors
///
/// Returns [`ReviewStoreError::MissingEnv`] when neither `XDG_DATA_HOME` nor
/// `HOME` resolves (fail-fast; no silent default location).
pub fn default_reviews_dir() -> Result<PathBuf, ReviewStoreError> {
    Ok(pohunek_paths::data_home()?
        .join(REVIEWS_APP_DIR)
        .join(REVIEWS_SUBDIR))
}

/// JSON-file review store: one file per review, temp-then-rename writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStore {
    dir: PathBuf,
}

impl ReviewStore {
    /// Creates a store rooted at `dir`. Does not touch the filesystem; the
    /// directory is created lazily on the first [`Self::save`].
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Returns the directory this store persists reviews under.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the JSON file path for `id`.
    #[must_use]
    pub fn path_for(&self, id: &ReviewId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Loads every review file in the store directory.
    ///
    /// A missing store directory (first run) yields an empty list, not an
    /// error. A file that fails to read or parse is surfaced as a
    /// [`ReviewLoadError`] entry alongside whichever reviews did load
    /// successfully — never a crash, never a silent skip.
    #[must_use]
    pub fn load_all(&self) -> Vec<Result<Review, ReviewLoadError>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                return vec![Err(ReviewLoadError {
                    path: self.dir.clone(),
                    message: err.to_string(),
                })];
            }
        };

        let mut results = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    results.push(Err(ReviewLoadError {
                        path: self.dir.clone(),
                        message: err.to_string(),
                    }));
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            results.push(load_review_file(&path));
        }
        results
    }

    /// Atomically persists `review` as `<id>.json` (temp file + rename).
    ///
    /// # Errors
    ///
    /// Returns [`ReviewStoreError`] when the directory cannot be created, the
    /// review cannot be serialized, or the temp file cannot be written,
    /// permission-restricted, or renamed into place. Because the write goes
    /// through a temp file that is only renamed over the real path on full
    /// success, a failure here never leaves a partially written review file:
    /// whatever was previously on disk (or nothing, for a first save) is
    /// unaffected.
    pub fn save(&self, review: &Review) -> Result<(), ReviewStoreError> {
        fs::create_dir_all(&self.dir).map_err(|source| ReviewStoreError::CreateDir {
            path: self.dir.clone(),
            source,
        })?;
        let path = self.path_for(&review.id);
        let temp_path = self.dir.join(format!("{}.json.tmp", review.id));
        let mut file = owner_private_write_options()
            .open(&temp_path)
            .map_err(|source| ReviewStoreError::Write {
                path: temp_path.clone(),
                source,
            })?;
        serde_json::to_writer_pretty(&mut file, review).map_err(|source| {
            ReviewStoreError::Serialize {
                id: review.id.clone(),
                source,
            }
        })?;
        file.write_all(b"\n")
            .and_then(|()| file.flush())
            .map_err(|source| ReviewStoreError::Write {
                path: temp_path.clone(),
                source,
            })?;
        set_owner_private_file_permissions(&temp_path)?;
        fs::rename(&temp_path, &path).map_err(|source| ReviewStoreError::Rename {
            path: path.clone(),
            source,
        })?;
        set_owner_private_file_permissions(&path)?;
        Ok(())
    }
}

fn load_review_file(path: &Path) -> Result<Review, ReviewLoadError> {
    let content = fs::read_to_string(path).map_err(|err| ReviewLoadError {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    serde_json::from_str(&content).map_err(|err| ReviewLoadError {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn owner_private_write_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(OWNER_PRIVATE_FILE_MODE)
    };
    options
}

#[cfg(unix)]
fn set_owner_private_file_permissions(path: &Path) -> Result<(), ReviewStoreError> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_PRIVATE_FILE_MODE)).map_err(
        |source| ReviewStoreError::Write {
            path: path.to_path_buf(),
            source,
        },
    )
}

#[cfg(not(unix))]
fn set_owner_private_file_permissions(_path: &Path) -> Result<(), ReviewStoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::model::{Review, ReviewSource};
    use super::ReviewStore;
    use crate::HostId;
    use protocol::SessionId;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-gui-core-review-store-{tag}-{}",
            std::process::id()
        ))
    }

    fn sample_review() -> Review {
        Review::new(
            ReviewSource::Session {
                host_id: HostId::new("host-1"),
                session_id: SessionId("s-1".to_owned()),
            },
            "project-1",
            "feature/x",
        )
    }

    #[test]
    fn load_all_on_a_missing_directory_returns_an_empty_list() {
        let store = ReviewStore::new(temp_dir("missing"));
        assert!(store.load_all().is_empty());
    }

    #[test]
    fn save_then_load_all_round_trips_the_review() {
        let store = ReviewStore::new(temp_dir("round-trip"));
        let review = sample_review();

        store.save(&review).expect("save review");
        let loaded = store.load_all();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].as_ref().expect("loaded review"), &review);
    }

    #[test]
    fn corrupt_review_file_surfaces_as_a_load_error_without_dropping_others() {
        let dir = temp_dir("corrupt");
        let store = ReviewStore::new(&dir);
        let review = sample_review();
        store.save(&review).expect("save good review");
        std::fs::write(dir.join("corrupt.json"), b"{not valid json").expect("write corrupt file");

        let loaded = store.load_all();

        assert_eq!(loaded.len(), 2);
        let good_count = loaded.iter().filter(|entry| entry.is_ok()).count();
        let bad_count = loaded.iter().filter(|entry| entry.is_err()).count();
        assert_eq!(good_count, 1);
        assert_eq!(bad_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn saved_review_file_is_owner_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let store = ReviewStore::new(temp_dir("perms"));
        let review = sample_review();
        store.save(&review).expect("save review");

        let metadata = std::fs::metadata(store.path_for(&review.id)).expect("file metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
}
