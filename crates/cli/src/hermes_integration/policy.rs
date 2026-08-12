// Rust guideline compliant 2026-08-12

#![expect(
    clippy::map_err_ignore,
    reason = "policy errors intentionally redact JSON and filesystem details"
)]
#![expect(
    clippy::too_many_arguments,
    reason = "validated policy reconstruction must receive every persisted policy field"
)]

use std::collections::BTreeSet;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use nix::unistd::{access, AccessFlags};
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use super::error::Error;
use super::target::ResolvedTarget;

/// The only policy schema understood by the embedded Hermes plugin.
const SCHEMA_VERSION: u32 = 1;
/// Plugin invocations must not occupy a delegated tool slot for longer than one minute.
pub(crate) const MAX_TIMEOUT_MS: u32 = 60_000;
/// Session creation leaves fifteen seconds for exact-state reconciliation.
pub(crate) const DEFAULT_REQUEST_TIMEOUT_MS: u32 = 45_000;
/// Policies created before request-timeout configuration used the SDK's five-second default.
const LEGACY_REQUEST_TIMEOUT_MS: u32 = 5_000;
/// This cap bounds one JSON tool result independently of protocol framing.
pub(crate) const MAX_OUTPUT_BYTES: u32 = 1_048_576;
/// Screens are bounded more tightly because they are repeated observation payloads.
pub(crate) const MAX_SCREEN_BYTES: u32 = 262_144;
/// The plugin has a deliberately small fixed slot pool to prevent local fan-out.
pub(crate) const MAX_CONCURRENCY: u8 = 8;
/// Policy files live in a Pohunek-owned namespace, never in a Hermes plugin tree.
const POLICY_DIR: [&str; 2] = ["policies", "hermes"];
/// Policy files are JSON because the embedded plugin validates this exact serde schema.
const POLICY_EXTENSION: &str = "json";
/// A persisted policy is small configuration; this prevents replacement-file abuse.
const MAX_PRIVATE_POLICY_BYTES: usize = 64 * 1024;
/// Group or other write access could replace a policy after it was validated.
const UNSAFE_WRITE_BITS: u32 = 0o022;
/// Sticky shared directories may safely occur in an absolute policy ancestor chain.
const STICKY_BIT: u32 = 0o1000;

/// Explicit access granted to the Hermes operator plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessMode {
    /// Permit observation-only tools.
    ReadOnly,
    /// Permit observation plus non-destructive session management.
    Manage,
    /// Permit every registered tool, including stop and remove.
    Full,
}

/// Caller acknowledgement required before persisting a wildcard host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WildcardConfirmation(bool);

impl WildcardConfirmation {
    /// Creates a confirmation from the caller's explicit command decision.
    #[must_use]
    pub(crate) const fn new(confirmed: bool) -> Self {
        Self(confirmed)
    }

    /// Reports whether the caller explicitly confirmed wildcard access.
    #[must_use]
    pub(crate) const fn is_confirmed(self) -> bool {
        self.0
    }
}

/// Inputs required to create one policy without security defaults.
#[derive(Debug, Clone)]
pub(crate) struct PolicyInput {
    /// The fixed absolute executable invoked by the plugin.
    pub(crate) pohunek_cli: PathBuf,
    /// The inclusive oldest Pohunek protocol version accepted by the plugin.
    pub(crate) protocol_min: i32,
    /// The inclusive newest Pohunek protocol version accepted by the plugin.
    pub(crate) protocol_max: i32,
    /// The explicit tool access mode.
    pub(crate) access_mode: AccessMode,
    /// Explicit host allowlist supplied by the caller.
    pub(crate) allowed_hosts: Vec<String>,
    /// Maximum one-tool wall-clock duration in milliseconds.
    pub(crate) tool_timeout_ms: u32,
    /// Maximum daemon response wait for session creation in milliseconds.
    pub(crate) request_timeout_ms: u32,
    /// Maximum returned tool-output bytes.
    pub(crate) max_output_bytes: u32,
    /// Maximum returned terminal-screen bytes.
    pub(crate) max_screen_bytes: u32,
    /// Maximum concurrent plugin tool invocations.
    pub(crate) max_concurrency: u8,
    /// The explicit acknowledgement required for a literal `*` host.
    pub(crate) wildcard_confirmation: WildcardConfirmation,
}

/// Versioned policy serialized for the embedded Hermes plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Policy {
    schema_version: u32,
    pohunek_cli: PathBuf,
    protocol_min: i32,
    protocol_max: i32,
    access_mode: AccessMode,
    allowed_hosts: Vec<Host>,
    tool_timeout_ms: u32,
    request_timeout_ms: u32,
    max_output_bytes: u32,
    max_screen_bytes: u32,
    max_concurrency: u8,
}

impl Policy {
    /// Validates all caller-provided policy fields and canonicalizes the CLI path.
    pub(crate) fn new(input: PolicyInput) -> Result<Self, Error> {
        let pohunek_cli = canonical_executable(&input.pohunek_cli)?;
        Self::from_parts(
            pohunek_cli,
            input.protocol_min,
            input.protocol_max,
            input.access_mode,
            input.allowed_hosts,
            input.tool_timeout_ms,
            input.request_timeout_ms,
            input.max_output_bytes,
            input.max_screen_bytes,
            input.max_concurrency,
            input.wildcard_confirmation,
        )
    }

    /// Parses and validates policy JSON previously selected by the caller.
    pub(crate) fn from_json(
        document: &[u8],
        wildcard_confirmation: WildcardConfirmation,
    ) -> Result<Self, Error> {
        let wire: PolicyWire =
            serde_json::from_slice(document).map_err(|_| Error::InvalidPolicy)?;
        if wire.schema_version != SCHEMA_VERSION {
            return Err(Error::UnsupportedPolicySchema);
        }
        let pohunek_cli = canonical_executable(&wire.pohunek_cli)?;
        Self::from_parts(
            pohunek_cli,
            wire.protocol_min,
            wire.protocol_max,
            wire.access_mode,
            wire.allowed_hosts,
            wire.tool_timeout_ms,
            wire.request_timeout_ms,
            wire.max_output_bytes,
            wire.max_screen_bytes,
            wire.max_concurrency,
            wildcard_confirmation,
        )
    }

    /// Loads one existing owner-private policy without following a replacement link.
    ///
    /// A stored wildcard was explicitly confirmed when the policy was installed.
    /// Callers must still require a fresh confirmation for a newly supplied
    /// wildcard while constructing an update policy.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the file, an ancestor, its size, or its
    /// schema does not satisfy the managed policy contract.
    pub(crate) fn load_private(path: &Path) -> Result<Self, Error> {
        let file = open_private_policy(path)?;
        let mut bytes = Vec::with_capacity(MAX_PRIVATE_POLICY_BYTES);
        file.take((MAX_PRIVATE_POLICY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(Error::from)?;
        if bytes.len() > MAX_PRIVATE_POLICY_BYTES {
            return Err(Error::InvalidPolicy);
        }
        Self::from_json(&bytes, WildcardConfirmation::new(true))
    }

    /// Returns the validated fixed Pohunek executable path.
    #[must_use]
    pub(crate) fn pohunek_cli(&self) -> &Path {
        &self.pohunek_cli
    }

    /// Returns the allowed hosts in deterministic caller order.
    #[must_use]
    pub(crate) fn allowed_hosts(&self) -> impl ExactSizeIterator<Item = &str> {
        self.allowed_hosts.iter().map(Host::as_str)
    }

    /// Returns the inclusive minimum compatible Pohunek protocol version.
    #[must_use]
    pub(crate) const fn protocol_min(&self) -> i32 {
        self.protocol_min
    }

    /// Returns the inclusive maximum compatible Pohunek protocol version.
    #[must_use]
    pub(crate) const fn protocol_max(&self) -> i32 {
        self.protocol_max
    }

    /// Returns the explicitly granted tool access mode.
    #[must_use]
    pub(crate) const fn access_mode(&self) -> AccessMode {
        self.access_mode
    }

    /// Returns the bounded per-tool timeout in milliseconds.
    #[must_use]
    pub(crate) const fn tool_timeout_ms(&self) -> u32 {
        self.tool_timeout_ms
    }

    /// Returns the bounded session-creation response timeout in milliseconds.
    #[must_use]
    pub(crate) const fn request_timeout_ms(&self) -> u32 {
        self.request_timeout_ms
    }

    /// Returns the bounded maximum tool output bytes.
    #[must_use]
    pub(crate) const fn max_output_bytes(&self) -> u32 {
        self.max_output_bytes
    }

    /// Returns the bounded maximum screen bytes.
    #[must_use]
    pub(crate) const fn max_screen_bytes(&self) -> u32 {
        self.max_screen_bytes
    }

    /// Returns the bounded maximum concurrent tool invocations.
    #[must_use]
    pub(crate) const fn max_concurrency(&self) -> u8 {
        self.max_concurrency
    }

    /// Serializes the validated policy for the embedded Python bootstrap.
    pub(crate) fn to_json(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(self).map_err(|_| Error::InvalidPolicy)
    }

    fn from_parts(
        pohunek_cli: PathBuf,
        protocol_min: i32,
        protocol_max: i32,
        access_mode: AccessMode,
        allowed_hosts: Vec<String>,
        tool_timeout_ms: u32,
        request_timeout_ms: u32,
        max_output_bytes: u32,
        max_screen_bytes: u32,
        max_concurrency: u8,
        wildcard_confirmation: WildcardConfirmation,
    ) -> Result<Self, Error> {
        if protocol_min < 1
            || protocol_max < 1
            || protocol_min > protocol_max
            || tool_timeout_ms == 0
            || tool_timeout_ms > MAX_TIMEOUT_MS
            || request_timeout_ms == 0
            || request_timeout_ms >= tool_timeout_ms
            || max_output_bytes == 0
            || max_output_bytes > MAX_OUTPUT_BYTES
            || max_screen_bytes == 0
            || max_screen_bytes > MAX_SCREEN_BYTES
            || max_concurrency == 0
            || max_concurrency > MAX_CONCURRENCY
        {
            return Err(Error::InvalidPolicy);
        }
        let allowed_hosts = validate_hosts(allowed_hosts, wildcard_confirmation)?;
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            pohunek_cli,
            protocol_min,
            protocol_max,
            access_mode,
            allowed_hosts,
            tool_timeout_ms,
            request_timeout_ms,
            max_output_bytes,
            max_screen_bytes,
            max_concurrency,
        })
    }
}

/// Returns the deterministic owner-private policy location for one Hermes home.
pub(crate) fn policy_path(config_root: &Path, target: &ResolvedTarget) -> Result<PathBuf, Error> {
    if !config_root.is_absolute() {
        return Err(Error::RelativePath);
    }
    let config_root = canonical_with_missing_tail(config_root)?;
    let path = canonical_policy_dir(&config_root)?;
    let home_key = Sha256::digest(target.hermes_home().as_os_str().as_bytes());
    let filename = format!("{home_key:x}.{POLICY_EXTENSION}");
    let policy = path.join(filename);
    if policy.starts_with(target.plugin_root()) {
        return Err(Error::UnsafePolicyPath);
    }
    Ok(policy)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Local,
    Name(String),
    Wildcard,
}

impl Host {
    fn parse(value: String, wildcard_confirmation: WildcardConfirmation) -> Result<Self, Error> {
        if value == "local" {
            return Ok(Self::Local);
        }
        if value == "*" {
            return wildcard_confirmation
                .is_confirmed()
                .then_some(Self::Wildcard)
                .ok_or(Error::WildcardConfirmationRequired);
        }
        if is_dns_name(&value) {
            Ok(Self::Name(value))
        } else {
            Err(Error::InvalidPolicy)
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Name(value) => value,
            Self::Wildcard => "*",
        }
    }
}

impl Serialize for Host {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyWire {
    schema_version: u32,
    pohunek_cli: PathBuf,
    protocol_min: i32,
    protocol_max: i32,
    access_mode: AccessMode,
    allowed_hosts: Vec<String>,
    tool_timeout_ms: u32,
    #[serde(default = "legacy_request_timeout_ms")]
    request_timeout_ms: u32,
    max_output_bytes: u32,
    max_screen_bytes: u32,
    max_concurrency: u8,
}

const fn legacy_request_timeout_ms() -> u32 {
    LEGACY_REQUEST_TIMEOUT_MS
}

fn canonical_executable(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidCliPath);
    }
    let canonical = fs::canonicalize(path).map_err(|_| Error::InvalidCliPath)?;
    let metadata = fs::metadata(&canonical).map_err(|_| Error::InvalidCliPath)?;
    if !metadata.is_file() || access(&canonical, AccessFlags::X_OK).is_err() {
        return Err(Error::InvalidCliPath);
    }
    Ok(canonical)
}

fn open_private_policy(path: &Path) -> Result<File, Error> {
    if !path.is_absolute() || !private_policy_ancestors(path)? {
        return Err(Error::UnsafePolicyPath);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(Error::from)?;
    let metadata = file.metadata().map_err(Error::from)?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(Error::UnsafePolicyPath);
    }
    Ok(file)
}

fn private_policy_ancestors(path: &Path) -> Result<bool, Error> {
    let uid = nix::unistd::Uid::effective().as_raw();
    let root_uid = fs::metadata(Path::new("/"))?.uid();
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Ok(false);
        }
    }
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    for ancestor in parent.ancestors() {
        let metadata = fs::metadata(ancestor)?;
        let mode = metadata.permissions().mode();
        let root_sticky_shared_directory = metadata.is_dir()
            && metadata.uid() == root_uid
            && mode & STICKY_BIT != 0
            && mode & UNSAFE_WRITE_BITS != 0;
        if !metadata.is_dir()
            || (metadata.uid() != uid && metadata.uid() != root_uid)
            || (mode & UNSAFE_WRITE_BITS != 0 && !root_sticky_shared_directory)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_hosts(
    allowed_hosts: Vec<String>,
    wildcard_confirmation: WildcardConfirmation,
) -> Result<Vec<Host>, Error> {
    if allowed_hosts.is_empty() {
        return Err(Error::InvalidPolicy);
    }
    let mut seen = BTreeSet::new();
    allowed_hosts
        .into_iter()
        .map(|host| {
            let parsed = Host::parse(host, wildcard_confirmation)?;
            seen.insert(parsed.as_str().to_owned())
                .then_some(parsed)
                .ok_or(Error::InvalidPolicy)
        })
        .collect()
}

fn is_dns_name(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 253
        || value.parse::<std::net::IpAddr>().is_ok()
        || value.ends_with('.')
    {
        return false;
    }
    value.split('.').all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    })
}

fn canonical_with_missing_tail(path: &Path) -> Result<PathBuf, Error> {
    let mut existing = path;
    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or(Error::UnsafePolicyPath)?;
            }
            Err(_) => return Err(Error::UnsafePolicyPath),
        }
    }
    let canonical = fs::canonicalize(existing).map_err(|_| Error::UnsafePolicyPath)?;
    let tail = path
        .strip_prefix(existing)
        .map_err(|_| Error::UnsafePolicyPath)?;
    Ok(canonical.join(tail))
}

fn canonical_policy_dir(config_root: &Path) -> Result<PathBuf, Error> {
    let path = POLICY_DIR
        .iter()
        .fold(config_root.to_owned(), |path, component| {
            path.join(component)
        });
    reject_symlink_components(&path)?;
    canonical_with_missing_tail(&path)
}

fn reject_symlink_components(path: &Path) -> Result<(), Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::UnsafePolicyPath)
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(Error::UnsafePolicyPath),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::hermes_integration::target::{ProfileName, TargetContext, TargetSelection};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl std::ops::Deref for Fixture {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.path) {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "cleanup fixture"
                );
            }
        }
    }

    fn temp_dir(tag: &str) -> Fixture {
        loop {
            let counter = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pohunek-hermes-policy-{tag}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                        .expect("set private mode");
                    return Fixture { path };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create isolated test directory: {error}"),
            }
        }
    }

    fn executable(root: &Path) -> PathBuf {
        let path = root.join("pohunek");
        fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write executable fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("set executable mode");
        path
    }

    fn input(cli: PathBuf) -> PolicyInput {
        PolicyInput {
            pohunek_cli: cli,
            protocol_min: 1,
            protocol_max: 2,
            access_mode: AccessMode::Manage,
            allowed_hosts: vec!["local".to_owned(), "peer-1.netbird".to_owned()],
            tool_timeout_ms: MAX_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_screen_bytes: MAX_SCREEN_BYTES,
            max_concurrency: MAX_CONCURRENCY,
            wildcard_confirmation: WildcardConfirmation::new(false),
        }
    }

    #[test]
    fn serializes_exact_python_schema_and_all_access_modes() {
        let root = temp_dir("schema");
        let cli = executable(&root);
        for access_mode in [AccessMode::ReadOnly, AccessMode::Manage, AccessMode::Full] {
            let mut input = input(cli.clone());
            input.access_mode = access_mode;
            let policy = Policy::new(input).expect("valid policy");
            let document = serde_json::to_value(&policy).expect("serialize policy");
            assert_eq!(
                document,
                json!({
                    "schema_version": 1,
                    "pohunek_cli": fs::canonicalize(&cli).expect("canonical cli"),
                    "protocol_min": 1,
                    "protocol_max": 2,
                    "access_mode": match access_mode {
                        AccessMode::ReadOnly => "read_only",
                        AccessMode::Manage => "manage",
                        AccessMode::Full => "full",
                    },
                    "allowed_hosts": ["local", "peer-1.netbird"],
                    "tool_timeout_ms": MAX_TIMEOUT_MS,
                    "request_timeout_ms": DEFAULT_REQUEST_TIMEOUT_MS,
                    "max_output_bytes": MAX_OUTPUT_BYTES,
                    "max_screen_bytes": MAX_SCREEN_BYTES,
                    "max_concurrency": MAX_CONCURRENCY,
                })
            );
            assert_eq!(policy.protocol_min(), 1);
            assert_eq!(policy.protocol_max(), 2);
            assert_eq!(policy.access_mode(), access_mode);
            assert_eq!(
                policy.allowed_hosts().collect::<Vec<_>>(),
                ["local", "peer-1.netbird"]
            );
            assert_eq!(policy.tool_timeout_ms(), MAX_TIMEOUT_MS);
            assert_eq!(policy.request_timeout_ms(), DEFAULT_REQUEST_TIMEOUT_MS);
            assert_eq!(policy.max_output_bytes(), MAX_OUTPUT_BYTES);
            assert_eq!(policy.max_screen_bytes(), MAX_SCREEN_BYTES);
            assert_eq!(policy.max_concurrency(), MAX_CONCURRENCY);
        }
    }

    #[test]
    fn loads_legacy_policy_with_the_historical_request_timeout() {
        let root = temp_dir("legacy-request-timeout");
        let cli = executable(&root);
        let policy = Policy::new(input(cli)).expect("policy");
        let mut document = serde_json::to_value(policy).expect("serialize policy");
        document
            .as_object_mut()
            .expect("policy object")
            .remove("request_timeout_ms");

        let loaded = Policy::from_json(
            serde_json::to_string(&document).expect("json").as_bytes(),
            WildcardConfirmation::new(false),
        )
        .expect("legacy policy");

        assert_eq!(loaded.request_timeout_ms(), LEGACY_REQUEST_TIMEOUT_MS);
    }

    #[test]
    fn rejects_all_policy_bounds_hosts_and_wildcards_without_confirmation() {
        let root = temp_dir("bounds");
        let cli = executable(&root);
        let invalid = [
            ("protocol_min", json!(0)),
            ("protocol_max", json!(0)),
            ("tool_timeout_ms", json!(0)),
            ("tool_timeout_ms", json!(MAX_TIMEOUT_MS + 1)),
            ("request_timeout_ms", json!(0)),
            ("request_timeout_ms", json!(MAX_TIMEOUT_MS)),
            ("max_output_bytes", json!(0)),
            ("max_output_bytes", json!(MAX_OUTPUT_BYTES + 1)),
            ("max_screen_bytes", json!(0)),
            ("max_screen_bytes", json!(MAX_SCREEN_BYTES + 1)),
            ("max_concurrency", json!(0)),
            ("max_concurrency", json!(MAX_CONCURRENCY + 1)),
        ];
        for (field, value) in invalid {
            let mut document =
                serde_json::to_value(Policy::new(input(cli.clone())).expect("policy"))
                    .expect("serialize");
            document[field] = value;
            assert_eq!(
                Policy::from_json(
                    serde_json::to_string(&document).expect("json").as_bytes(),
                    WildcardConfirmation::new(false)
                ),
                Err(Error::InvalidPolicy),
                "{field}"
            );
        }

        let mut reversed = input(cli.clone());
        reversed.protocol_min = 2;
        reversed.protocol_max = 1;
        assert_eq!(Policy::new(reversed), Err(Error::InvalidPolicy));

        for hosts in [
            Vec::new(),
            vec!["local".to_owned(), "local".to_owned()],
            vec!["100.64.0.1".to_owned()],
            vec!["bad..name".to_owned()],
        ] {
            let mut invalid = input(cli.clone());
            invalid.allowed_hosts = hosts;
            assert_eq!(Policy::new(invalid), Err(Error::InvalidPolicy));
        }
        let mut mixed_case = input(cli.clone());
        mixed_case.allowed_hosts = vec!["UPPER.example".to_owned()];
        assert_eq!(
            Policy::new(mixed_case)
                .expect("Python-compatible mixed-case hostname")
                .allowed_hosts()
                .collect::<Vec<_>>(),
            ["UPPER.example"]
        );
        let mut trailing_hyphen = input(cli.clone());
        trailing_hyphen.allowed_hosts = vec!["peer-.netbird".to_owned()];
        assert_eq!(
            Policy::new(trailing_hyphen)
                .expect("Python-compatible trailing hostname hyphen")
                .allowed_hosts()
                .collect::<Vec<_>>(),
            ["peer-.netbird"]
        );

        let mut wildcard = input(cli.clone());
        wildcard.allowed_hosts = vec!["*".to_owned()];
        assert_eq!(
            Policy::new(wildcard.clone()),
            Err(Error::WildcardConfirmationRequired)
        );
        wildcard.wildcard_confirmation = WildcardConfirmation::new(true);
        assert_eq!(
            Policy::new(wildcard)
                .expect("confirmed wildcard")
                .allowed_hosts()
                .collect::<Vec<_>>(),
            ["*"]
        );
    }

    #[test]
    fn rejects_bad_schema_unknown_fields_and_cli_paths() {
        let root = temp_dir("invalid");
        let cli = executable(&root);
        let document = serde_json::to_value(Policy::new(input(cli.clone())).expect("policy"))
            .expect("serialize");
        let mut bad_schema = document.clone();
        bad_schema["schema_version"] = json!(2);
        assert_eq!(
            Policy::from_json(
                serde_json::to_string(&bad_schema).expect("json").as_bytes(),
                WildcardConfirmation::new(false)
            ),
            Err(Error::UnsupportedPolicySchema)
        );
        let mut unknown = document;
        unknown["unexpected"] = json!(true);
        assert_eq!(
            Policy::from_json(
                serde_json::to_string(&unknown).expect("json").as_bytes(),
                WildcardConfirmation::new(false)
            ),
            Err(Error::InvalidPolicy)
        );

        let mut relative = input(PathBuf::from("pohunek"));
        assert_eq!(Policy::new(relative.clone()), Err(Error::InvalidCliPath));
        relative.pohunek_cli = root.join("not-executable");
        fs::write(&relative.pohunek_cli, b"fixture").expect("write fixture");
        assert_eq!(Policy::new(relative), Err(Error::InvalidCliPath));
        let group_only = root.join("group-only-executable");
        fs::write(&group_only, b"fixture").expect("write fixture");
        fs::set_permissions(&group_only, fs::Permissions::from_mode(0o010))
            .expect("set group execute mode");
        assert_eq!(Policy::new(input(group_only)), Err(Error::InvalidCliPath));
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o100))
            .expect("set owner execute mode");
        Policy::new(input(cli.clone())).expect("valid policy");
        symlink(&cli, root.join("pohunek-link")).expect("create cli symlink");
        let mut linked = input(root.join("pohunek-link"));
        linked.allowed_hosts = vec!["local".to_owned()];
        assert_eq!(
            Policy::new(linked)
                .expect("canonical symlink executable")
                .pohunek_cli(),
            fs::canonicalize(cli).expect("canonical cli")
        );
    }

    #[test]
    fn policy_path_is_deterministic_and_never_enters_plugin_tree() {
        let root = temp_dir("path");
        let hermes = root.join("hermes");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let config = root.join("config");
        for path in [&hermes, &home, &workspace, &config] {
            fs::create_dir(path).expect("create fixture directory");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("set private mode");
        }
        let target = TargetContext::new(hermes, home, vec![workspace])
            .expect("context")
            .resolve(TargetSelection::Profile(ProfileName::default()))
            .expect("target");
        let first = policy_path(&config, &target).expect("policy path");
        let second = policy_path(&config, &target).expect("same policy path");
        assert_eq!(first, second);
        assert!(!first.starts_with(target.plugin_root()));
        assert_eq!(
            first.extension().and_then(|value| value.to_str()),
            Some("json")
        );
        assert_eq!(
            policy_path(target.plugin_root(), &target),
            Err(Error::UnsafePolicyPath)
        );
    }

    #[test]
    fn rejects_symlinks_in_each_existing_policy_component() {
        for (index, component) in POLICY_DIR.iter().enumerate() {
            let root = temp_dir(component);
            let hermes = root.join("hermes");
            let home = root.join("home");
            let workspace = root.join("workspace");
            let config = root.join("config");
            let outside = root.join("outside");
            for path in [&hermes, &home, &workspace, &config, &outside] {
                fs::create_dir(path).expect("create fixture directory");
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .expect("set private mode");
            }
            let target = TargetContext::new(hermes, home, vec![workspace])
                .expect("context")
                .resolve(TargetSelection::Profile(ProfileName::default()))
                .expect("target");
            let mut parent = config.clone();
            for previous in &POLICY_DIR[..index] {
                parent.push(previous);
                fs::create_dir(&parent).expect("create policy component");
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
                    .expect("set private mode");
            }
            symlink(&outside, parent.join(component)).expect("create policy symlink");
            assert_eq!(
                policy_path(&config, &target),
                Err(Error::UnsafePolicyPath),
                "{component}"
            );
        }
    }

    #[test]
    fn errors_are_redacted() {
        let rendered = Error::InvalidCliPath.to_string();
        assert!(!rendered.contains("/tmp"));
        assert!(!Error::InvalidCliPath.recovery_hint().contains("/tmp"));
    }
}
