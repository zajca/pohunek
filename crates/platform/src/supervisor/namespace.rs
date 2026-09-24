//! Derives installation namespaces and the native job names built from them.
//!
//! One installation (a user plus its canonical state and runtime roots) owns
//! exactly one namespace `<ns>`: the first 12 lowercase hex characters of
//! SHA-256 over `"<uid>\0<state root>\0<runtime root>"`. Every launchd label and
//! systemd unit a backend creates embeds `<ns>`, and every name a backend reads
//! back is parsed strictly against it, so jobs of another installation, another
//! product, or a malformed name are never adopted or retired.
//!
//! | Target  | Daemon                                  | Worker generation |
//! |---------|-----------------------------------------|-------------------|
//! | launchd | `io.github.zajca.pohunek.<ns>.daemon`   | `io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>` |
//! | systemd | `pohunek-<ns>-daemon.service`           | `pohunek-<ns>-worker-<session-id>-<generation>.service` |
//!
//! Session IDs and generations never contain `.` and generations never
//! contain `-`, so both separators split unambiguously.

use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Error, ServiceId};

// Rust guideline compliant 2026-09-24

/// Hex characters kept from the namespace digest.
///
/// 48 bits keep accidental collisions between one user's installations out of
/// reach while leaving launchd labels and unit names short.
pub const NAMESPACE_LEN: usize = 12;

/// Reverse-DNS prefix of every launchd label, owned by the project repository.
const LABEL_PREFIX: &str = "io.github.zajca.pohunek.";
/// Prefix of every systemd unit and slice name.
const UNIT_PREFIX: &str = "pohunek-";
/// launchd label suffix naming the daemon job.
const DAEMON_LABEL_SUFFIX: &str = ".daemon";
/// launchd label segment introducing a worker generation.
const WORKER_LABEL_SEGMENT: &str = ".worker.";
/// systemd unit segment naming the daemon unit.
const DAEMON_UNIT_SUFFIX: &str = "-daemon.service";
/// systemd unit segment introducing a worker generation.
const WORKER_UNIT_SEGMENT: &str = "-worker-";
/// systemd service unit suffix.
const SERVICE_SUFFIX: &str = ".service";
/// systemd slice suffix grouping every worker of one namespace.
const SESSIONS_SLICE_SUFFIX: &str = "-sessions.slice";

/// Installation namespace embedded in every native job name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace(String);

impl Namespace {
    /// Derives the namespace of one installation.
    ///
    /// `state_root` and `runtime_root` must be the canonical application state
    /// and runtime directories; callers canonicalize them so equivalent
    /// spellings of one installation always produce the same namespace.
    #[must_use]
    pub fn derive(uid: u32, state_root: &Path, runtime_root: &Path) -> Self {
        let mut digest = Sha256::new();
        digest.update(uid.to_string().as_bytes());
        digest.update([0]);
        digest.update(state_root.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(runtime_root.as_os_str().as_encoded_bytes());
        let hex = digest
            .finalize()
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .take(NAMESPACE_LEN)
            .map(|nibble| char::from_digit(u32::from(nibble), 16).expect("nibble is below 16"))
            .collect();
        Self(hex)
    }

    /// Parses a previously derived namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] unless the value is exactly
    /// [`NAMESPACE_LEN`] lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.len() == NAMESPACE_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(Error::InvalidServiceId(value.to_owned()))
        }
    }

    /// Returns the namespace characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the launchd label of the daemon login agent.
    #[must_use]
    pub fn daemon_label(&self) -> String {
        format!("{LABEL_PREFIX}{}{DAEMON_LABEL_SUFFIX}", self.0)
    }

    /// Returns the launchd label of one worker generation.
    #[must_use]
    pub fn worker_label(&self, key: &WorkerKey) -> String {
        format!(
            "{LABEL_PREFIX}{}{WORKER_LABEL_SEGMENT}{}.{}",
            self.0, key.session_id, key.generation
        )
    }

    /// Parses a launchd worker label of this namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] for a foreign prefix, another
    /// namespace, the daemon label, or a malformed session ID or generation.
    pub fn parse_worker_label(&self, label: &str) -> Result<WorkerKey, Error> {
        let rest = label
            .strip_prefix(LABEL_PREFIX)
            .and_then(|rest| rest.strip_prefix(self.0.as_str()))
            .and_then(|rest| rest.strip_prefix(WORKER_LABEL_SEGMENT))
            .ok_or_else(|| Error::InvalidServiceId(label.to_owned()))?;
        let (session_id, generation) = rest
            .split_once('.')
            .ok_or_else(|| Error::InvalidServiceId(label.to_owned()))?;
        WorkerKey::new(session_id, generation)
            .map_err(|_invalid_key| Error::InvalidServiceId(label.to_owned()))
    }

    /// Returns the systemd unit of the daemon.
    #[must_use]
    pub fn daemon_unit(&self) -> String {
        format!("{UNIT_PREFIX}{}{DAEMON_UNIT_SUFFIX}", self.0)
    }

    /// Returns the transient systemd unit of one worker generation.
    #[must_use]
    pub fn worker_unit(&self, key: &WorkerKey) -> String {
        format!(
            "{UNIT_PREFIX}{}{WORKER_UNIT_SEGMENT}{}-{}{SERVICE_SUFFIX}",
            self.0, key.session_id, key.generation
        )
    }

    /// Returns the `ListUnitsByPatterns` glob matching this namespace's workers.
    #[must_use]
    pub fn worker_unit_pattern(&self) -> String {
        format!(
            "{UNIT_PREFIX}{}{WORKER_UNIT_SEGMENT}*{SERVICE_SUFFIX}",
            self.0
        )
    }

    /// Parses a systemd worker unit of this namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] for a foreign prefix, another
    /// namespace, the daemon unit, or a malformed session ID or generation.
    pub fn parse_worker_unit(&self, unit: &str) -> Result<WorkerKey, Error> {
        let rest = unit
            .strip_prefix(UNIT_PREFIX)
            .and_then(|rest| rest.strip_prefix(self.0.as_str()))
            .and_then(|rest| rest.strip_prefix(WORKER_UNIT_SEGMENT))
            .and_then(|rest| rest.strip_suffix(SERVICE_SUFFIX))
            .ok_or_else(|| Error::InvalidServiceId(unit.to_owned()))?;
        // Generations never contain `-`, so the last dash separates them from
        // the session ID (which itself starts with `s-`).
        let (session_id, generation) = rest
            .rsplit_once('-')
            .ok_or_else(|| Error::InvalidServiceId(unit.to_owned()))?;
        WorkerKey::new(session_id, generation)
            .map_err(|_invalid_key| Error::InvalidServiceId(unit.to_owned()))
    }

    /// Returns the systemd slice grouping this namespace's workers.
    #[must_use]
    pub fn sessions_slice(&self) -> String {
        format!("{UNIT_PREFIX}{}{SESSIONS_SLICE_SUFFIX}", self.0)
    }
}

impl std::fmt::Display for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One worker runtime generation of one managed session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerKey {
    session_id: String,
    generation: String,
}

impl WorkerKey {
    /// Validates a managed session ID and a daemon-issued generation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] when either part violates
    /// `pohunek_paths::valid_worker_session_id` or
    /// `pohunek_paths::valid_worker_generation`.
    pub fn new(
        session_id: impl Into<String>,
        generation: impl Into<String>,
    ) -> Result<Self, Error> {
        let session_id = session_id.into();
        let generation = generation.into();
        if pohunek_paths::valid_worker_session_id(&session_id).is_none()
            || pohunek_paths::valid_worker_generation(&generation).is_none()
        {
            return Err(Error::InvalidServiceId(format!(
                "{session_id}.{generation}"
            )));
        }
        Ok(Self {
            session_id,
            generation,
        })
    }

    /// Returns the managed session ID.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the runtime generation.
    #[must_use]
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Returns the backend-neutral service ID `<session-id>.<generation>`.
    #[must_use]
    pub fn service_id(&self) -> ServiceId {
        ServiceId::parse(format!("{}.{}", self.session_id, self.generation))
            .expect("validated session IDs and generations form a safe service ID")
    }

    /// Parses a backend-neutral worker service ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] unless the ID is exactly
    /// `<session-id>.<generation>`.
    pub fn from_service_id(id: &ServiceId) -> Result<Self, Error> {
        let (session_id, generation) = id
            .as_str()
            .split_once('.')
            .ok_or_else(|| Error::InvalidServiceId(id.to_string()))?;
        Self::new(session_id, generation)
    }
}

impl std::fmt::Display for WorkerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.session_id, self.generation)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    const SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN2";

    fn namespace() -> Namespace {
        Namespace::derive(
            501,
            Path::new("/Users/u/.local/state/pohunek"),
            Path::new("/Users/u/Library/Caches/TemporaryItems/pohunek"),
        )
    }

    fn key() -> WorkerKey {
        WorkerKey::new(SESSION, "abcd2345").expect("valid key")
    }

    #[test]
    fn derivation_is_deterministic_and_input_sensitive() {
        let state = Path::new("/home/u/.local/state/pohunek");
        let runtime = Path::new("/run/user/1000/pohunek");
        let first = Namespace::derive(1000, state, runtime);
        assert_eq!(first, Namespace::derive(1000, state, runtime));
        assert_eq!(first.as_str().len(), NAMESPACE_LEN);
        assert!(first
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        assert_ne!(first, Namespace::derive(1001, state, runtime));
        assert_ne!(first, Namespace::derive(1000, Path::new("/other"), runtime));
        assert_ne!(first, Namespace::derive(1000, state, Path::new("/other")));
        // The separator keeps shifted boundaries between inputs distinct.
        assert_ne!(
            Namespace::derive(1, Path::new("/a"), Path::new("/b")),
            Namespace::derive(1, Path::new("/a\0/b"), Path::new(""))
        );
    }

    #[test]
    fn derivation_matches_a_known_digest() {
        // The documented byte layout: decimal UID, NUL, state root, NUL, runtime root.
        let expected = {
            let mut digest = Sha256::new();
            digest.update(b"1000\0/s\0/r");
            let mut hex = String::new();
            for byte in digest.finalize() {
                write!(hex, "{byte:02x}").expect("writing to a String is infallible");
            }
            hex.truncate(NAMESPACE_LEN);
            hex
        };
        assert_eq!(
            Namespace::derive(1000, Path::new("/s"), Path::new("/r")).as_str(),
            expected
        );
    }

    #[test]
    fn namespaces_parse_only_lowercase_hex() {
        let namespace = namespace();
        assert_eq!(
            Namespace::parse(namespace.as_str()).expect("round trip"),
            namespace
        );
        for invalid in ["", "abc", "ABCDEF012345", "abcdef01234g", "abcdef0123456"] {
            assert!(Namespace::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn launchd_labels_round_trip() {
        let namespace = namespace();
        let label = namespace.worker_label(&key());
        assert_eq!(
            label,
            format!("io.github.zajca.pohunek.{namespace}.worker.{SESSION}.abcd2345")
        );
        assert_eq!(
            namespace.parse_worker_label(&label).expect("own label"),
            key()
        );
        assert_eq!(
            namespace.daemon_label(),
            format!("io.github.zajca.pohunek.{namespace}.daemon")
        );
    }

    #[test]
    fn launchd_labels_reject_foreign_malformed_and_other_namespace_names() {
        let namespace = namespace();
        let other = Namespace::parse("0123456789ab").expect("valid namespace");
        for label in [
            other.worker_label(&key()),
            namespace.daemon_label(),
            format!("com.example.{namespace}.worker.{SESSION}.abcd2345"),
            format!("io.github.zajca.pohunek.{namespace}.worker.{SESSION}"),
            format!("io.github.zajca.pohunek.{namespace}.worker.{SESSION}.ABCD2345"),
            format!("io.github.zajca.pohunek.{namespace}.worker.{SESSION}.abcd2345.x"),
            format!("io.github.zajca.pohunek.{namespace}.worker.s-x.abcd2345"),
            format!("io.github.zajca.pohunek.{namespace}.worker../x.abcd2345"),
            String::new(),
        ] {
            assert!(
                matches!(
                    namespace.parse_worker_label(&label),
                    Err(Error::InvalidServiceId(_))
                ),
                "{label}"
            );
        }
    }

    #[test]
    fn systemd_units_round_trip() {
        let namespace = namespace();
        let unit = namespace.worker_unit(&key());
        assert_eq!(
            unit,
            format!("pohunek-{namespace}-worker-{SESSION}-abcd2345.service")
        );
        assert_eq!(namespace.parse_worker_unit(&unit).expect("own unit"), key());
        let numeric = WorkerKey::new("s-42", "zzzzzzzz").expect("numeric session");
        assert_eq!(
            namespace
                .parse_worker_unit(&namespace.worker_unit(&numeric))
                .expect("numeric unit"),
            numeric
        );
        assert_eq!(
            namespace.daemon_unit(),
            format!("pohunek-{namespace}-daemon.service")
        );
        assert_eq!(
            namespace.sessions_slice(),
            format!("pohunek-{namespace}-sessions.slice")
        );
        assert_eq!(
            namespace.worker_unit_pattern(),
            format!("pohunek-{namespace}-worker-*.service")
        );
    }

    #[test]
    fn systemd_units_reject_foreign_malformed_and_other_namespace_names() {
        let namespace = namespace();
        let other = Namespace::parse("0123456789ab").expect("valid namespace");
        for unit in [
            other.worker_unit(&key()),
            namespace.daemon_unit(),
            format!("pohunek-session@{SESSION}.service"),
            format!("pohunek-{namespace}-worker-{SESSION}.service"),
            format!("pohunek-{namespace}-worker-{SESSION}-abcd2345.socket"),
            format!("pohunek-{namespace}-worker-{SESSION}-abcd-2345.service"),
            format!("pohunek-{namespace}-worker-s-x-abcd2345.service"),
            format!("other-{namespace}-worker-{SESSION}-abcd2345.service"),
        ] {
            assert!(
                matches!(
                    namespace.parse_worker_unit(&unit),
                    Err(Error::InvalidServiceId(_))
                ),
                "{unit}"
            );
        }
    }

    #[test]
    fn worker_keys_round_trip_through_service_ids() {
        let id = key().service_id();
        assert_eq!(id.as_str(), format!("{SESSION}.abcd2345"));
        assert_eq!(WorkerKey::from_service_id(&id).expect("round trip"), key());
        for invalid in ["s-42", "s-42.abcd2345.x", "x-42.abcd2345", "s-42.abcd234"] {
            let id = ServiceId::parse(invalid).expect("syntactically safe");
            assert!(WorkerKey::from_service_id(&id).is_err(), "{invalid}");
        }
    }
}
