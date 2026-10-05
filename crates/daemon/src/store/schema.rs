//! Schema versioning and startup migration of the metadata store.
//!
//! Every line of `metadata.jsonl` carries a `schema_version`, whatever its record
//! kind. A per-line version (rather than a header line) keeps each record
//! self-describing: an older daemon that skips an unknown line never strips a
//! store-wide marker, and a hand-edited or partially written file is still judged
//! record by record. A line without the field was written before versioning and
//! is schema [`UNVERSIONED_LINE_SCHEMA`].
//!
//! Migrations run on the raw [`serde_json::Value`] of each line, before typed
//! deserialization: the typed deserializers ignore unknown fields, so a typed
//! round trip would destroy exactly the legacy fields a later migration step
//! needs. [`Store::migrate_to_current`] runs once at daemon startup, before any
//! reconciliation or store write, and copies the store to
//! `<store>.pre-schema-<old>` before the first migrating write. Every other store
//! access refuses a store whose schema is not the current one
//! ([`StoreSchemaError`]), so no mutation can rewrite data it does not understand.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use pohunek_platform::filesystem::{MoveOutcome, StageOutcome, TrustedDir};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::error::DaemonError;

use super::{
    fs_error_to_io, legacy_binding, parent_directory, Store, MAX_METADATA_STORE_BYTES,
    OWNER_PRIVATE_DIRECTORY_MODE, OWNER_PRIVATE_FILE_MODE, TEMP_SEQUENCE,
};

/// Schema version every line written by this daemon carries.
///
/// Bump it, and add a [`MIGRATIONS`] step, whenever the serialized shape of a
/// persisted record kind changes.
pub const STORE_SCHEMA_VERSION: u32 = 2;

/// Schema assumed for a line that has no `schema_version` field.
pub(super) const UNVERSIONED_LINE_SCHEMA: u32 = 1;

/// JSON key holding a line's schema version.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// One schema step: rewrites a record from schema `from` to schema `from + 1`.
pub(super) struct Migration {
    /// Schema the step upgrades from.
    pub(super) from: u32,
    /// Rewrites one record object in place; the runner sets the new version.
    pub(super) apply: fn(&mut Map<String, Value>),
}

/// Every kept schema step, ordered by `from`. A store at any `from` listed here
/// reaches [`STORE_SCHEMA_VERSION`] by applying the steps in sequence.
pub(super) const MIGRATIONS: &[Migration] = &[Migration {
    from: 1,
    apply: migrate_v1_to_v2,
}];

/// Schema 1 to 2: introduces the per-line version and the native-launch shape.
///
/// Resume bindings written before the native launch spec existed are mapped by
/// [`legacy_binding::migrate_record`]; every other record keeps its fields.
fn migrate_v1_to_v2(record: &mut Map<String, Value>) {
    legacy_binding::migrate_record(record);
}

/// A metadata store whose schema this daemon cannot use as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreSchemaError {
    /// The store was written by a newer daemon.
    #[error(
        "metadata store {} has schema version {found}, newer than the schema version \
         {supported} this daemon supports; run a pohunek release that supports it \
         (a downgrade is not supported)",
        path.display()
    )]
    NewerThanBinary {
        /// Store file.
        path: PathBuf,
        /// Highest schema version found in the store.
        found: u32,
        /// Schema version this daemon writes.
        supported: u32,
    },
    /// The store is older than every migration this daemon keeps.
    #[error(
        "metadata store {} has schema version {found} and this daemon (schema version \
         {supported}) keeps no migration path from it; migrate it with an intermediate \
         pohunek release or remove the store",
        path.display()
    )]
    NoMigrationPath {
        /// Store file.
        path: PathBuf,
        /// Oldest schema version found in the store.
        found: u32,
        /// Schema version this daemon writes.
        supported: u32,
    },
    /// The store needs the startup migration before it can be used.
    #[error(
        "metadata store {} has schema version {found} and must be migrated to schema \
         version {supported} at daemon startup before it is used",
        path.display()
    )]
    MigrationRequired {
        /// Store file.
        path: PathBuf,
        /// Oldest schema version found in the store.
        found: u32,
        /// Schema version this daemon writes.
        supported: u32,
    },
    /// A line carries a `schema_version` that is not a schema number.
    #[error(
        "metadata store {} line {line} has an invalid schema_version",
        path.display()
    )]
    InvalidVersion {
        /// Store file.
        path: PathBuf,
        /// One-based line number.
        line: usize,
    },
}

impl StoreSchemaError {
    /// The schema error carried by `error`, when it came from the store.
    #[must_use]
    pub fn from_io(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
    }
}

impl From<StoreSchemaError> for io::Error {
    fn from(error: StoreSchemaError) -> Self {
        Self::new(io::ErrorKind::InvalidData, error)
    }
}

/// Outcome of [`Store::migrate_to_current`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaMigration {
    /// No store file, or every record already has the current schema.
    UpToDate,
    /// The store was rewritten at the current schema.
    Migrated {
        /// Oldest schema version that was found.
        from: u32,
        /// Schema version written.
        to: u32,
        /// Number of records carried over.
        records: usize,
        /// Backup of the pre-migration store.
        backup: PathBuf,
    },
}

/// One non-empty line of the store.
pub(super) enum ParsedLine<'a> {
    /// A JSON object with a resolved schema version.
    Record {
        text: &'a str,
        value: Value,
        version: u32,
    },
    /// A line that is not a JSON object; it has no schema to judge.
    Corrupt { text: &'a str, error: String },
}

/// What the schema versions of a whole store call for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SchemaState {
    /// Every record has the current schema.
    Current,
    /// Records with an older schema exist and every step to current is kept.
    Migratable { oldest: u32 },
}

/// Splits `content` into lines and resolves each line's schema version.
///
/// # Errors
///
/// Returns [`StoreSchemaError::InvalidVersion`] for a `schema_version` that is
/// not an integer that fits a schema number.
pub(super) fn parse_lines<'a>(
    path: &std::path::Path,
    content: &'a str,
) -> Result<Vec<ParsedLine<'a>>, StoreSchemaError> {
    let mut lines = Vec::new();
    for (index, text) in content.lines().enumerate() {
        if text.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(text) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                lines.push(ParsedLine::Corrupt {
                    text,
                    error: "record is not a JSON object".to_owned(),
                });
                continue;
            }
            Err(error) => {
                lines.push(ParsedLine::Corrupt {
                    text,
                    error: error.to_string(),
                });
                continue;
            }
        };
        let version = match value.get(SCHEMA_VERSION_KEY) {
            None => UNVERSIONED_LINE_SCHEMA,
            Some(raw) => raw
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| StoreSchemaError::InvalidVersion {
                    path: path.to_path_buf(),
                    line: index + 1,
                })?,
        };
        lines.push(ParsedLine::Record {
            text,
            value,
            version,
        });
    }
    Ok(lines)
}

/// Judges the schema versions of every record against this daemon.
///
/// # Errors
///
/// Returns [`StoreSchemaError::NewerThanBinary`] when any record is newer than
/// [`STORE_SCHEMA_VERSION`], and [`StoreSchemaError::NoMigrationPath`] when the
/// oldest record has no kept chain of steps to it.
pub(super) fn classify(
    path: &std::path::Path,
    lines: &[ParsedLine<'_>],
) -> Result<SchemaState, StoreSchemaError> {
    let versions = lines.iter().filter_map(|line| match line {
        ParsedLine::Record { version, .. } => Some(*version),
        ParsedLine::Corrupt { .. } => None,
    });
    let (oldest, newest) = versions.fold((u32::MAX, 0), |(oldest, newest), version| {
        (oldest.min(version), newest.max(version))
    });
    if newest > STORE_SCHEMA_VERSION {
        return Err(StoreSchemaError::NewerThanBinary {
            path: path.to_path_buf(),
            found: newest,
            supported: STORE_SCHEMA_VERSION,
        });
    }
    if oldest >= STORE_SCHEMA_VERSION {
        return Ok(SchemaState::Current);
    }
    if !path_exists(MIGRATIONS, STORE_SCHEMA_VERSION, oldest) {
        return Err(StoreSchemaError::NoMigrationPath {
            path: path.to_path_buf(),
            found: oldest,
            supported: STORE_SCHEMA_VERSION,
        });
    }
    Ok(SchemaState::Migratable { oldest })
}

/// Whether every step from `version` up to `target` is kept in `migrations`.
fn path_exists(migrations: &[Migration], target: u32, version: u32) -> bool {
    (version..target).all(|from| migrations.iter().any(|migration| migration.from == from))
}

/// Applies every step in `migrations` from `version` up to `target` and stamps
/// each intermediate and the final schema.
fn apply_steps(migrations: &[Migration], target: u32, value: &mut Value, version: u32) {
    let Some(record) = value.as_object_mut() else {
        return;
    };
    for from in version..target {
        if let Some(migration) = migrations.iter().find(|migration| migration.from == from) {
            (migration.apply)(record);
        }
        record.insert(SCHEMA_VERSION_KEY.to_owned(), Value::from(from + 1));
    }
}

/// Stamps `value` with the current schema version.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] when `value` is not a JSON object.
pub(super) fn stamp_current_schema(value: &mut Value) -> io::Result<()> {
    let record = value.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata record did not serialize to a JSON object",
        )
    })?;
    record.insert(
        SCHEMA_VERSION_KEY.to_owned(),
        Value::from(STORE_SCHEMA_VERSION),
    );
    Ok(())
}

/// Renders the current-schema store body for `lines`.
///
/// Deterministic: the same input lines always yield the same bytes, which is
/// what lets [`Store::is_schema_migration_of`] recognise a migrated store.
fn migrated_body(lines: Vec<ParsedLine<'_>>, capacity: usize) -> io::Result<(String, usize)> {
    let mut body = String::with_capacity(capacity);
    let mut records = 0;
    for line in lines {
        match line {
            ParsedLine::Record {
                mut value, version, ..
            } => {
                apply_steps(MIGRATIONS, STORE_SCHEMA_VERSION, &mut value, version);
                let encoded = serde_json::to_string(&value)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                body.push_str(&encoded);
                records += 1;
            }
            ParsedLine::Corrupt { text, .. } => body.push_str(text),
        }
        body.push('\n');
    }
    Ok((body, records))
}

impl Store {
    /// Migrates the store to the current schema, backing it up first.
    ///
    /// Run once at daemon startup, before reconciliation and before any other
    /// store access. The pre-migration bytes are copied to
    /// `<store>.pre-schema-<oldest>` (owner-only, fsynced) before the rewrite; an
    /// existing backup is kept, so a rerun after an interrupted migration neither
    /// loses the original nor overwrites it. Records that are not JSON objects
    /// are carried over verbatim.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] wrapping a [`StoreSchemaError`] (see
    /// [`StoreSchemaError::from_io`]) when the store is newer than this daemon or
    /// has no kept migration path, and an I/O error when the store cannot be
    /// read, backed up or rewritten. The store is left untouched on every error
    /// before the final atomic rename.
    pub fn migrate_to_current(&self) -> io::Result<SchemaMigration> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(content) = self.read_content()? else {
            return Ok(SchemaMigration::UpToDate);
        };
        let lines = parse_lines(&self.path, &content)?;
        let oldest = match classify(&self.path, &lines)? {
            SchemaState::Current => return Ok(SchemaMigration::UpToDate),
            SchemaState::Migratable { oldest } => oldest,
        };

        let backup = self.write_backup(content.as_bytes(), oldest)?;

        let (body, records) = migrated_body(lines, content.len())?;
        self.commit_body(&body)?.with_value(());
        info!(
            store.path = %self.path.display(),
            store.schema_from = oldest,
            store.schema_to = STORE_SCHEMA_VERSION,
            store.backup = %backup.display(),
            "migrated the metadata store to the current schema"
        );
        Ok(SchemaMigration::Migrated {
            from: oldest,
            to: STORE_SCHEMA_VERSION,
            records,
            backup,
        })
    }

    /// Whether the store is exactly the schema migration of bytes that hash to
    /// `original_sha256` (lowercase hex SHA-256).
    ///
    /// A pre-migration backup (`<store>.pre-schema-<n>`) must hash to
    /// `original_sha256` and migrating its bytes must reproduce the store
    /// byte for byte. A store edited after the backup was taken, or a backup of
    /// an already edited store, therefore never matches. Lets a fingerprint
    /// taken before the schema migration (the legacy migration manifest's
    /// `store_sha256`) be validated against the original bytes afterwards.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the store or a backup cannot be read.
    pub fn is_schema_migration_of(&self, original_sha256: &str) -> io::Result<bool> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(current) = self.read_content()? else {
            return Ok(false);
        };
        let store_name = self.path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "metadata store has no filename",
            )
        })?;
        let directory =
            TrustedDir::open_absolute(parent_directory(&self.path), OWNER_PRIVATE_DIRECTORY_MODE)
                .map_err(fs_error_to_io)?;
        for schema in 1..STORE_SCHEMA_VERSION {
            let backup_name = pohunek_paths::schema_backup_name(store_name, schema);
            let bytes = match directory.read_file(
                &backup_name,
                OWNER_PRIVATE_FILE_MODE,
                MAX_METADATA_STORE_BYTES,
            ) {
                Ok(bytes) => bytes,
                Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => continue,
                Err(error) => return Err(fs_error_to_io(error)),
            };
            if format!("{:x}", Sha256::digest(&bytes)) != original_sha256 {
                continue;
            }
            let Ok(original) = String::from_utf8(bytes) else {
                continue;
            };
            let lines = parse_lines(&self.path, &original)?;
            if !matches!(
                classify(&self.path, &lines)?,
                SchemaState::Migratable { .. }
            ) {
                continue;
            }
            if migrated_body(lines, original.len())?.0 == current {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Copies `bytes` to `<store>.pre-schema-<schema>` unless a backup exists.
    ///
    /// The copy is written to a temporary name and moved into place without
    /// replacement, so a crash never leaves a partial file under the backup name.
    fn write_backup(&self, bytes: &[u8], schema: u32) -> io::Result<PathBuf> {
        let store_name = self.path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "metadata store has no filename",
            )
        })?;
        let backup_name = pohunek_paths::schema_backup_name(store_name, schema);
        let backup_path = parent_directory(&self.path).join(&backup_name);

        let directory =
            TrustedDir::open_absolute(parent_directory(&self.path), OWNER_PRIVATE_DIRECTORY_MODE)
                .map_err(fs_error_to_io)?;
        if directory
            .open_file(&backup_name, OWNER_PRIVATE_FILE_MODE)
            .map_err(fs_error_to_io)?
            .is_some()
        {
            warn!(
                store.backup = %backup_path.display(),
                "keeping the existing pre-migration backup"
            );
            return Ok(backup_path);
        }

        let temporary = pohunek_paths::schema_backup_temp_name(
            store_name,
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        );
        let identity = directory
            .create_file(&temporary, bytes, OWNER_PRIVATE_FILE_MODE)
            .map_err(fs_error_to_io)?;
        match directory
            .move_no_replace(&temporary, &directory, &backup_name)
            .map_err(fs_error_to_io)?
        {
            MoveOutcome::Moved => {}
            MoveOutcome::DestinationExists => {
                // A concurrent writer created the backup first; keep it and drop ours.
                if let Ok(StageOutcome::Staged(entry)) =
                    directory.stage_random(&temporary, ".pohunek-backup-stale-", identity)
                {
                    let _ = entry.remove();
                }
            }
            _ => {
                return Err(io::Error::other("unexpected backup move outcome"));
            }
        }
        Ok(backup_path)
    }
}

/// Migrates the store at `path` as part of daemon startup.
///
/// # Errors
///
/// Returns [`DaemonError::StoreSchema`] when the store is newer than this daemon
/// or has no kept migration path, and [`DaemonError::StoreMigration`] when it
/// cannot be read, backed up or rewritten.
pub fn migrate_at_startup(path: &std::path::Path) -> Result<SchemaMigration, DaemonError> {
    Store::new(path.to_path_buf())
        .migrate_to_current()
        .map_err(|source| match StoreSchemaError::from_io(&source) {
            Some(schema) => DaemonError::StoreSchema(schema.clone()),
            None => DaemonError::StoreMigration {
                path: path.to_path_buf(),
                source,
            },
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::{
        apply_steps, migrate_at_startup, path_exists, Migration, SchemaMigration, Store,
        StoreSchemaError, MIGRATIONS, STORE_SCHEMA_VERSION,
    };
    use crate::error::DaemonError;
    use crate::store::{ResumeBinding, WorktreeStatus};

    /// A store written by the v0.33.0 daemon (schema 1) through its own code: one
    /// line per record kind, with the legacy resume fields (`resume_mode`,
    /// `ref_kind`, `resumable`, `fork_*`) that later releases replaced.
    const V0_33_0_STORE: &str = include_str!("fixtures/v0.33.0/metadata.jsonl");

    fn store_dir(tag: &str) -> PathBuf {
        crate::test_support::thread_scoped_dir(&format!("pohunek-schema-{tag}-"))
    }

    fn write_private(path: &Path, bytes: &str) {
        fs::write(path, bytes).expect("write store fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("make store fixture private");
    }

    fn raw_lines(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .expect("read store")
            .lines()
            .map(|line| serde_json::from_str(line).expect("store line is json"))
            .collect()
    }

    fn backup_path(store: &Path, schema: u32) -> PathBuf {
        let mut name = store.file_name().expect("store name").to_os_string();
        name.push(format!(".pre-schema-{schema}"));
        store.with_file_name(name)
    }

    fn store_with(tag: &str, content: &str) -> (Store, PathBuf) {
        let path = store_dir(tag).join("metadata.jsonl");
        write_private(&path, content);
        (Store::new(path.clone()), path)
    }

    fn schema_error(error: &std::io::Error) -> &StoreSchemaError {
        StoreSchemaError::from_io(error).expect("error carries a StoreSchemaError")
    }

    #[test]
    fn a_newer_store_is_refused_by_every_access_and_left_untouched() {
        let content = format!(
            "{}{{\"kind\":\"project\",\"schema_version\":{},\"future_field\":true}}\n",
            V0_33_0_STORE.replace("\"schema_version\":1", "\"schema_version\":2"),
            STORE_SCHEMA_VERSION + 1
        );
        let (store, path) = store_with("newer", &content);

        let expected = StoreSchemaError::NewerThanBinary {
            path: path.clone(),
            found: STORE_SCHEMA_VERSION + 1,
            supported: STORE_SCHEMA_VERSION,
        };
        assert_eq!(
            schema_error(&store.load_resume().expect_err("load refuses")),
            &expected
        );
        let binding = raw_resume_binding();
        assert_eq!(
            schema_error(&store.record_resume(&binding).expect_err("write refuses")),
            &expected
        );
        assert_eq!(
            schema_error(&store.migrate_to_current().expect_err("migrate refuses")),
            &expected
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read store"),
            content,
            "a refused store is never rewritten"
        );
        assert!(
            !backup_path(&path, STORE_SCHEMA_VERSION).exists(),
            "a refused store is never backed up"
        );
    }

    fn raw_resume_binding() -> ResumeBinding {
        let line = V0_33_0_STORE.lines().next().expect("resume line");
        serde_json::from_str::<super::super::Record>(line)
            .map(|record| match record {
                super::super::Record::Resume(binding) => binding,
                _ => unreachable!("the first fixture line is the resume binding"),
            })
            .expect("parse the fixture resume line")
    }

    #[test]
    fn the_error_message_names_both_versions() {
        let message = StoreSchemaError::NewerThanBinary {
            path: PathBuf::from("/data/metadata.jsonl"),
            found: 9,
            supported: 2,
        }
        .to_string();
        assert!(message.contains("schema version 9"), "{message}");
        assert!(message.contains("schema version 2"), "{message}");
        assert!(message.contains("/data/metadata.jsonl"), "{message}");
    }

    #[test]
    fn an_older_store_without_a_migration_path_is_refused() {
        let (store, path) = store_with(
            "no-path",
            "{\"kind\":\"project\",\"schema_version\":0,\"git_common_dir\":\"/r/.git\"}\n",
        );
        let expected = StoreSchemaError::NoMigrationPath {
            path: path.clone(),
            found: 0,
            supported: STORE_SCHEMA_VERSION,
        };
        assert_eq!(
            schema_error(&store.load_projects().expect_err("load refuses")),
            &expected
        );
        assert_eq!(
            schema_error(&store.migrate_to_current().expect_err("migrate refuses")),
            &expected
        );
        assert!(!backup_path(&path, 0).exists());
    }

    #[test]
    fn a_non_numeric_schema_version_is_refused() {
        let (store, path) = store_with(
            "invalid-version",
            "{\"kind\":\"project\",\"schema_version\":\"two\"}\n",
        );
        assert_eq!(
            schema_error(&store.load_projects().expect_err("load refuses")),
            &StoreSchemaError::InvalidVersion { path, line: 1 }
        );
    }

    #[test]
    fn an_unmigrated_store_is_refused_until_the_startup_migration_runs() {
        let (store, path) = store_with("unmigrated", V0_33_0_STORE);
        let error = store.load_resume().expect_err("load refuses");
        assert_eq!(
            schema_error(&error),
            &StoreSchemaError::MigrationRequired {
                path: path.clone(),
                found: 1,
                supported: STORE_SCHEMA_VERSION,
            }
        );
        let binding = raw_resume_binding();
        store
            .record_resume(&binding)
            .expect_err("a mutation cannot rewrite an unmigrated store");
        assert_eq!(
            fs::read_to_string(&path).expect("read store"),
            V0_33_0_STORE,
            "the refused mutation left the store byte-identical"
        );
    }

    #[test]
    fn a_corrupt_unversioned_line_is_skipped_and_migration_keeps_it_verbatim() {
        let content = format!("{{not json at all\n{V0_33_0_STORE}");
        let (store, path) = store_with("corrupt-kept", &content);

        let outcome = store.migrate_to_current().expect("migrate");

        assert!(matches!(
            outcome,
            SchemaMigration::Migrated {
                from: 1,
                records: 4,
                ..
            }
        ));
        let migrated = fs::read_to_string(&path).expect("read store");
        assert_eq!(migrated.lines().next(), Some("{not json at all"));
        assert_eq!(
            store.load_projects().expect("load after migration").len(),
            1,
            "the corrupt line does not block loading the rest"
        );
    }

    #[test]
    fn the_v0_33_0_fixture_migrates_with_a_backup_and_loads_with_its_contents() {
        let (store, path) = store_with("fixture", V0_33_0_STORE);

        let outcome = store.migrate_to_current().expect("migrate");

        let backup = backup_path(&path, 1);
        assert_eq!(
            outcome,
            SchemaMigration::Migrated {
                from: 1,
                to: STORE_SCHEMA_VERSION,
                records: 4,
                backup: backup.clone(),
            }
        );
        assert_eq!(
            fs::read_to_string(&backup).expect("read backup"),
            V0_33_0_STORE,
            "the backup holds the pre-migration bytes"
        );
        assert_eq!(
            fs::metadata(&backup)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the backup is owner-only"
        );

        let lines = raw_lines(&path);
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(line["schema_version"], json!(STORE_SCHEMA_VERSION));
        }
        let resume = &lines[0];
        assert_eq!(resume["kind"], "resume");
        assert!(
            resume.get("resume_mode").is_none() && resume.get("forkable").is_none(),
            "the legacy fields are replaced by the native launch spec: {resume}"
        );
        assert_eq!(
            resume["native_launch"]["resume_args"],
            json!([{"literal": "resume"}, "reference"])
        );

        let resumes = store.load_resume().expect("load resume");
        assert_eq!(resumes.len(), 1);
        let binding = &resumes[0];
        assert_eq!(binding.session_id, "s-fixture-resume");
        assert_eq!(binding.name.as_deref(), Some("fixture resume"));
        assert_eq!(binding.agent, "claude");
        assert_eq!(binding.cwd, PathBuf::from("/workspace/project"));
        assert_eq!(
            binding.native_session_id.as_deref(),
            Some("native-fixture-1")
        );
        assert_eq!(binding.project_id.as_deref(), Some("p-fixture"));
        assert_eq!(binding.is_linked_worktree, Some(true));
        assert_eq!(
            binding.metadata.get("owner").map(String::as_str),
            Some("fixture")
        );
        assert_eq!(binding.args, ["--model", "sonnet"]);
        assert_eq!(binding.input_rules.submit_delay_ms, 150);

        let worktrees = store.load_worktrees().expect("load worktrees");
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].session_id, "s-fixture-worktree");
        assert_eq!(worktrees[0].repository, PathBuf::from("/workspace/project"));
        assert_eq!(worktrees[0].branch, "feat/fixture");
        assert_eq!(worktrees[0].base_branch, "main");
        assert_eq!(worktrees[0].status, WorktreeStatus::Active);

        let projects = store.load_projects().expect("load projects");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].custom_name.as_deref(), Some("Fixture Project"));
        assert_eq!(projects[0].repo_root, PathBuf::from("/workspace/project"));
        assert_eq!(
            projects[0].origin_url.as_deref(),
            Some("https://github.com/example/repo.git")
        );

        let sessions = store.load_sessions().expect("load sessions");
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.schema_version, STORE_SCHEMA_VERSION);
        assert_eq!(session.session_id, "s-fixture-session");
        assert_eq!(session.info.id.0, "s-fixture-session");
        assert_eq!(session.info.pid, 4242);
        assert_eq!(
            session.info.native_session_id.as_deref(),
            Some("native-fixture-2")
        );
        assert_eq!(session.runtime.worker_id.as_deref(), Some("w-fixture"));
        assert_eq!(session.runtime.generation.as_deref(), Some("g-fixture"));
        let recovery = session.recovery.as_ref().expect("recovery binding");
        assert_eq!(
            recovery.native_session_id.as_deref(),
            Some("native-fixture-2")
        );
    }

    #[test]
    fn migration_is_idempotent_and_leaves_a_migrated_store_alone() {
        let (store, path) = store_with("idempotent", V0_33_0_STORE);
        store.migrate_to_current().expect("first migration");
        let migrated = fs::read_to_string(&path).expect("read store");

        assert_eq!(
            store.migrate_to_current().expect("second migration"),
            SchemaMigration::UpToDate
        );
        assert_eq!(fs::read_to_string(&path).expect("read store"), migrated);
        assert_eq!(
            fs::read_to_string(backup_path(&path, 1)).expect("read backup"),
            V0_33_0_STORE,
            "the backup still holds the original"
        );
    }

    #[test]
    fn a_store_written_at_the_current_schema_needs_no_migration_or_backup() {
        let (store, path) = store_with("current", "");
        fs::remove_file(&path).expect("remove empty store");
        store
            .record_resume(&raw_resume_binding())
            .expect("write current record");

        assert_eq!(
            store.migrate_to_current().expect("migrate"),
            SchemaMigration::UpToDate
        );
        assert!(!backup_path(&path, 1).exists());
        assert_eq!(
            raw_lines(&path)[0]["schema_version"],
            json!(STORE_SCHEMA_VERSION),
            "every written record carries the current schema"
        );
    }

    fn sha256_hex(content: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(content.as_bytes()))
    }

    #[test]
    fn a_migrated_store_is_recognised_as_the_migration_of_its_original_bytes() {
        let (store, path) = store_with("recognise", V0_33_0_STORE);
        let original = sha256_hex(V0_33_0_STORE);
        assert!(
            !store.is_schema_migration_of(&original).expect("check"),
            "an unmigrated store has no backup yet"
        );

        store.migrate_to_current().expect("migrate");

        assert!(store.is_schema_migration_of(&original).expect("check"));
        assert!(
            !store
                .is_schema_migration_of(&sha256_hex("something else\n"))
                .expect("check"),
            "a different fingerprint never matches"
        );

        // A store edited after the migration no longer derives from the backup.
        let mut edited = fs::read_to_string(&path).expect("read store");
        edited.push_str("{not json\n");
        write_private(&path, &edited);
        assert!(!store.is_schema_migration_of(&original).expect("check"));
    }

    #[test]
    fn a_stale_backup_does_not_vouch_for_a_store_edited_before_the_rerun() {
        let (store, path) = store_with("stale-backup", V0_33_0_STORE);
        store.fail_next_write_before_rename();
        store.migrate_to_current().expect_err("interrupted");
        // The store is edited between the interrupted run and the rerun.
        let edited = format!("{V0_33_0_STORE}{{not json\n");
        write_private(&path, &edited);

        store.migrate_to_current().expect("rerun");

        assert!(
            !store
                .is_schema_migration_of(&sha256_hex(V0_33_0_STORE))
                .expect("check"),
            "the kept backup holds the original bytes but the store derives from edited ones"
        );
    }

    #[test]
    fn a_missing_store_needs_no_migration() {
        let store = Store::new(store_dir("missing").join("metadata.jsonl"));
        assert_eq!(
            store.migrate_to_current().expect("migrate"),
            SchemaMigration::UpToDate
        );
    }

    #[test]
    fn an_interrupted_migration_keeps_the_backup_and_the_rerun_completes() {
        let (store, path) = store_with("interrupted", V0_33_0_STORE);
        store.fail_next_write_before_rename();

        store
            .migrate_to_current()
            .expect_err("the injected failure interrupts the rewrite");

        assert_eq!(
            fs::read_to_string(&path).expect("read store"),
            V0_33_0_STORE,
            "the store is untouched before the rename"
        );
        let backup = backup_path(&path, 1);
        assert_eq!(
            fs::read_to_string(&backup).expect("read backup"),
            V0_33_0_STORE,
            "the backup survives the interruption"
        );

        let outcome = store.migrate_to_current().expect("rerun");

        assert!(matches!(outcome, SchemaMigration::Migrated { from: 1, .. }));
        assert_eq!(
            fs::read_to_string(&backup).expect("read backup"),
            V0_33_0_STORE,
            "the rerun keeps the original backup"
        );
        for line in raw_lines(&path) {
            assert_eq!(line["schema_version"], json!(STORE_SCHEMA_VERSION));
        }
        assert_eq!(store.load_resume().expect("load").len(), 1);
    }

    #[test]
    fn an_existing_backup_is_never_overwritten() {
        let (store, path) = store_with("keep-backup", V0_33_0_STORE);
        let backup = backup_path(&path, 1);
        write_private(&backup, "earlier backup\n");

        store.migrate_to_current().expect("migrate");

        assert_eq!(
            fs::read_to_string(&backup).expect("read backup"),
            "earlier backup\n"
        );
    }

    #[test]
    fn every_file_a_migration_adds_is_a_backup_artifact_the_purge_recognises() {
        let (store, path) = store_with("artifacts", V0_33_0_STORE);
        let dir = path.parent().expect("store dir").to_path_buf();
        let before: Vec<_> = fs::read_dir(&dir)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();

        store.migrate_to_current().expect("migrate");

        let added: Vec<_> = fs::read_dir(&dir)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| !before.contains(name))
            .collect();
        assert_eq!(added.len(), 1, "{added:?}");
        assert!(pohunek_paths::is_schema_backup_artifact(
            pohunek_paths::METADATA_STORE_NAME,
            added[0].to_str().expect("utf-8")
        ));
    }

    #[test]
    fn migration_leaves_no_temporary_backup_file_behind() {
        let (store, path) = store_with("no-temp", V0_33_0_STORE);
        write_private(&backup_path(&path, 1), "earlier backup\n");

        store.migrate_to_current().expect("migrate");

        let leftovers: Vec<_> = fs::read_dir(path.parent().expect("store dir"))
            .expect("list store dir")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| name.to_string_lossy().contains("pre-schema-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_mixed_version_store_is_backed_up_under_its_oldest_schema() {
        let project = format!(
            "{{\"kind\":\"project\",\"schema_version\":{STORE_SCHEMA_VERSION},\"git_common_dir\":\"/r/.git\",\"repo_root\":\"/r\",\"is_bare\":false,\"source\":\"auto\",\"added_at\":\"t\",\"last_used_at\":\"t\"}}\n"
        );
        let content = format!("{V0_33_0_STORE}{project}");
        let (store, path) = store_with("mixed", &content);

        let outcome = store.migrate_to_current().expect("migrate");

        assert!(matches!(
            outcome,
            SchemaMigration::Migrated {
                from: 1,
                records: 5,
                ..
            }
        ));
        assert!(backup_path(&path, 1).exists());
        assert_eq!(store.load_projects().expect("load").len(), 2);
    }

    #[test]
    fn steps_compose_from_any_kept_schema_and_keep_unknown_fields() {
        fn add_a(record: &mut serde_json::Map<String, serde_json::Value>) {
            record.insert("a".to_owned(), json!(1));
        }
        fn add_b(record: &mut serde_json::Map<String, serde_json::Value>) {
            record.insert("b".to_owned(), json!(2));
        }
        let steps = [
            Migration {
                from: 1,
                apply: add_a,
            },
            Migration {
                from: 2,
                apply: add_b,
            },
        ];

        let mut from_one = json!({"kind": "resume", "legacy": "kept"});
        apply_steps(&steps, 3, &mut from_one, 1);
        assert_eq!(
            from_one,
            json!({"kind": "resume", "legacy": "kept", "a": 1, "b": 2, "schema_version": 3})
        );

        let mut from_two = json!({"kind": "resume", "schema_version": 2});
        apply_steps(&steps, 3, &mut from_two, 2);
        assert_eq!(
            from_two,
            json!({"kind": "resume", "b": 2, "schema_version": 3}),
            "only the steps above the record's schema run"
        );

        assert!(path_exists(&steps, 3, 1));
        assert!(path_exists(&steps, 3, 2));
        assert!(path_exists(&steps, 3, 3));
        assert!(!path_exists(&steps, 3, 0));
        assert!(!path_exists(&steps[1..], 3, 1), "a gap breaks the chain");
    }

    #[test]
    fn the_kept_steps_reach_the_current_schema_from_every_older_version() {
        for version in 1..STORE_SCHEMA_VERSION {
            assert!(
                path_exists(MIGRATIONS, STORE_SCHEMA_VERSION, version),
                "no migration path from schema {version} to {STORE_SCHEMA_VERSION}"
            );
        }
    }

    #[test]
    fn startup_maps_a_newer_store_to_a_typed_daemon_error() {
        let (_store, path) = store_with(
            "startup-newer",
            &format!(
                "{{\"kind\":\"project\",\"schema_version\":{}}}\n",
                STORE_SCHEMA_VERSION + 1
            ),
        );

        match migrate_at_startup(&path) {
            Err(DaemonError::StoreSchema(StoreSchemaError::NewerThanBinary {
                found,
                supported,
                ..
            })) => {
                assert_eq!(found, STORE_SCHEMA_VERSION + 1);
                assert_eq!(supported, STORE_SCHEMA_VERSION);
            }
            other => panic!("expected a newer-schema startup error, got {other:?}"),
        }
    }

    #[test]
    fn startup_migrates_an_older_store_and_accepts_a_missing_one() {
        let (_store, path) = store_with("startup-older", V0_33_0_STORE);
        assert!(matches!(
            migrate_at_startup(&path).expect("migrate at startup"),
            SchemaMigration::Migrated { from: 1, .. }
        ));
        assert_eq!(
            migrate_at_startup(&store_dir("startup-missing").join("metadata.jsonl"))
                .expect("missing store"),
            SchemaMigration::UpToDate
        );
    }
}
