//! Per-host agent profiles (Part C).
//!
//! A **profile** is a host-authored `~/.config/pohunek/agents/<name>.toml` that
//! *extends* a compiled **base kind** (`shell`/`codex`/`claude`/`hermes`) with overrides for
//! the launch program/args, the PTY env, and the input rules (resume + manifest
//! overrides land in C2). The wire/in-repo `agent` is a **name**: it resolves —
//! charset-guarded, fail-closed — to a host profile or a bare base kind, never to a
//! program. `program`/`args`/`env` come ONLY from a host profile or a base kind,
//! never from the wire or a repo (the A.5 boundary).
//!
//! Resolution ([`ProfileRegistry::resolve_agent`]) is the 4-step chain: A.2.1
//! charset guard → profile file → bare base kind → `agent_profile_not_found`.
//!
//! A profile whose base runtime is served by an installed package binds that
//! package explicitly with the `package` and `digest` keys and resolves from
//! exactly that digest; a profile over a built-in base carries neither key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use package::registry::RetainedDigests;
use package::PackageDigest;
use protocol::{ErrorClass, PackageId, ProtocolError, RuntimeId};
use serde::Deserialize;
use tracing::warn;

use super::host::{ProfileInputs, RevisionKeys, RuntimeDefinition, RuntimeHost, ServedBy};
use super::{InputRules, NativeArgs, NativeSessionLaunch, SessionRefKind};
use crate::detect::Manifest;
use crate::project::config::validate_name;

/// A parsed `agents/<name>.toml`. `deny_unknown_fields` keeps the surface tight so
/// a typo is a loud error rather than a silently-ignored key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    /// Base kind to extend: `shell` | `codex` | `claude` | `hermes`.
    base: String,
    /// Package serving `base`; set together with `digest` exactly when a
    /// package, not a built-in, serves the base runtime.
    #[serde(default)]
    package: Option<String>,
    /// Archive digest (`sha256:<hex>`) of the package version the profile
    /// launches from, whatever the registry selects now.
    #[serde(default)]
    digest: Option<String>,
    /// Launch program (PATH name or absolute path); defaults to the base program.
    #[serde(default)]
    program: Option<String>,
    /// Launch args appended to the program.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Extra PTY env (every `POHUNEK_`-prefixed key is stripped on load — reserved).
    #[serde(default)]
    env: HashMap<String, EnvValue>,
    /// Input-framing override; absent ⇒ the base kind's defaults.
    #[serde(default)]
    input_rules: Option<RawInputRules>,
    /// Native recovery override; absent ⇒ inherit the base kind's native-session
    /// launch spec (or non-resumable for a shell).
    #[serde(default)]
    resume: Option<RawResume>,
    /// Detection-manifest override name, resolved from `agents/manifests/<name>.toml`
    /// under the same charset + containment guard as a profile file.
    #[serde(default)]
    manifest: Option<String>,
}

/// A profile env value. Deserializing a non-string fails with a fixed message
/// instead of serde's default, which echoes the offending value.
#[derive(Clone)]
struct EnvValue(String);

impl std::fmt::Debug for EnvValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvValue(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StringOnly;

        impl<'de> serde::de::Visitor<'de> for StringOnly {
            type Value = EnvValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EnvValue, E> {
                Ok(EnvValue(v.to_owned()))
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<EnvValue, E> {
                Err(E::custom("environment values must be strings"))
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<EnvValue, E> {
                Err(E::custom("environment values must be strings"))
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<EnvValue, E> {
                Err(E::custom("environment values must be strings"))
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<EnvValue, E> {
                Err(E::custom("environment values must be strings"))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<EnvValue, A::Error> {
                Err(serde::de::Error::custom(
                    "environment values must be strings",
                ))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, _: A) -> Result<EnvValue, A::Error> {
                Err(serde::de::Error::custom(
                    "environment values must be strings",
                ))
            }
        }

        deserializer.deserialize_any(StringOnly)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputRules {
    #[serde(default)]
    bracketed_paste: Option<bool>,
    #[serde(default)]
    submit_delay_ms: Option<u64>,
}

/// A `[resume]` table: either `resumable = false` (no native recovery) or a
/// complete native-session launch spec. A spec is never merged with the base
/// kind's argv, so a fork shape can never be combined with a different resume
/// shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResume {
    /// `false` disables native recovery; absent or `true` requires a spec.
    #[serde(default)]
    resumable: Option<bool>,
    /// `id` | `path`: the kind of native reference both operations consume.
    #[serde(default)]
    reference_kind: Option<String>,
    /// Resume argv template with exactly one whole-token `{reference}`.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Fork argv template with exactly one whole-token `{reference}`; absent ⇒
    /// the profile cannot fork natively.
    #[serde(default)]
    fork_args: Option<Vec<String>>,
}

/// Host-profile launch overrides. Present on a [`ResolvedAgent`] only when a
/// profile file backed the name; a bare base kind carries `None` and launches
/// exactly as the compiled base adapter.
///
/// Not `PartialEq`/`Eq`: `manifest` holds a compiled `regex::Regex`, which has no
/// meaningful structural equality. Tests assert on individual fields instead.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedProfile {
    /// Launch program (resolved on PATH at launch, like a base kind's program).
    pub program: String,
    /// Launch args.
    pub args: Vec<String>,
    /// Non-secret PTY env, with every `POHUNEK_`-prefixed key already stripped.
    pub env: Vec<(String, String)>,
    /// Input-rules override; `None` ⇒ inherit the base kind's rules.
    pub input_rules: Option<InputRules>,
    /// Resolved native-session launch spec; `None` ⇒ not resumable and not
    /// forkable. Authoritative for a profile (does NOT fall back to the base kind
    /// when `None`).
    pub native: Option<NativeSessionLaunch>,
    /// Parsed detection-manifest override; `None` ⇒ inherit the base kind's manifest.
    pub manifest: Option<Manifest>,
    /// Digest of the profile's launch inputs; the source of its revision.
    pub inputs: ProfileInputs,
}

/// The resolution of an agent NAME on this host: its base kind plus optional
/// host-profile overrides. Not `PartialEq`/`Eq` (see [`ResolvedProfile`]).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedAgent {
    /// The resolved agent name (a profile name, or a bare base-kind name).
    pub name: String,
    /// The base kind this resolves to (drives detection/resume/handshake env).
    pub base: RuntimeId,
    /// The definition of the base runtime.
    pub definition: Arc<RuntimeDefinition>,
    /// Host-profile overrides; `None` for a bare base kind.
    pub profile: Option<ResolvedProfile>,
}

impl ResolvedAgent {
    /// Return the effective native-session launch spec for this resolved agent.
    #[must_use]
    pub(crate) fn native_launch(&self) -> Option<NativeSessionLaunch> {
        self.profile.as_ref().map_or_else(
            || self.definition.native().cloned(),
            |profile| profile.native.clone(),
        )
    }

    /// The launch inputs of the host profile this agent resolved from; `None`
    /// for a bare runtime.
    #[must_use]
    pub(crate) fn profile_inputs(&self) -> Option<&ProfileInputs> {
        self.profile.as_ref().map(|profile| &profile.inputs)
    }

    /// The program the agent launches: the profile's, else the base runtime's.
    #[must_use]
    pub(crate) fn program(&self) -> &str {
        self.profile.as_ref().map_or_else(
            || self.definition.program().as_str(),
            |profile| profile.program.as_str(),
        )
    }

    /// The launch arguments frozen into the resume snapshot: the profile's,
    /// else the base descriptor's fixed arguments (never the host shell
    /// command's, which is not part of the runtime).
    #[must_use]
    pub(crate) fn snapshot_args(&self) -> Vec<String> {
        self.profile.as_ref().map_or_else(
            || self.definition.default_args().to_vec(),
            |profile| profile.args.clone(),
        )
    }
}

/// Loads + resolves host agent profiles from `<config_dir>/agents`.
///
/// Resolution is on demand (per `session.new`), but the **owner-only security gate**
/// (C.5) runs once at construction: the whole `agents/` tree (the dir itself **and**
/// `agents/manifests/`) must be owned by the daemon user and not group/world-writable,
/// or the entire host-profile layer is disabled fail-closed (a stored `None` dir).
/// Each resolved profile/manifest file is additionally canonicalize-and-contain
/// checked at load to defeat a symlink that escapes the tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProfileRegistry {
    /// The `agents/` directory, or `None` when the host-config layer is disabled
    /// (unconfigured, absent, or failing the owner-only gate).
    dir: Option<PathBuf>,
    /// Resolves the base runtime a name or profile extends.
    runtimes: RuntimeHost,
    /// Host-local key that turns profile launch inputs into revisions.
    revision_keys: Arc<RevisionKeys>,
}

impl ProfileRegistry {
    /// A registry over the built-in runtimes with the host's login shell.
    #[cfg(test)]
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self::with_runtimes(dir, RuntimeHost::default())
    }

    /// The runtime host this registry resolves base runtimes from.
    pub(crate) fn runtimes(&self) -> &RuntimeHost {
        &self.runtimes
    }

    pub(crate) fn with_runtimes(dir: Option<PathBuf>, runtimes: RuntimeHost) -> Self {
        // C.5: gate the whole tree once at boot. An insecure `agents/` OR
        // `agents/manifests/` disables every host profile (fail-closed) — a
        // world-writable manifests dir means no manifest can be trusted, so no
        // profile may load. An absent dir is not "insecure" (there are simply no
        // profiles); the gate only rejects a present-but-unsafe dir.
        let dir = dir.filter(|dir| {
            let manifests = dir.join("manifests");
            let secure = dir_is_owner_secure(dir) && dir_is_owner_secure(&manifests);
            if !secure {
                warn!(
                    dir = %dir.display(),
                    "agent profiles directory is not owner-secure (wrong owner or group/world-writable); ignoring all host agent profiles"
                );
            }
            secure
        });
        Self {
            dir,
            runtimes,
            revision_keys: Arc::default(),
        }
    }

    /// Keys profile revisions with a secret kept in the host-state directory
    /// `state_dir`. Without one, revisions are unavailable (typed error).
    #[must_use]
    pub(crate) fn with_revision_state_dir(mut self, state_dir: Option<PathBuf>) -> Self {
        self.revision_keys = Arc::new(RevisionKeys::new(state_dir));
        self
    }

    /// The revision keys of this host.
    pub(crate) fn revision_keys(&self) -> &RevisionKeys {
        &self.revision_keys
    }

    /// The revision of the host profile `agent` resolved from, or `None` for a
    /// bare runtime. This is the owner's approval step; its caller arrives
    /// with the locally approved `HostShare` (#82).
    ///
    /// # Errors
    ///
    /// Returns `agent_profile_revision_unavailable` when the host's revision
    /// key cannot be read or created.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the approval caller arrives with #82")
    )]
    pub(crate) fn revision_of(
        &self,
        agent: &ResolvedAgent,
    ) -> Result<Option<super::host::ProfileRevision>, ProtocolError> {
        agent
            .profile_inputs()
            .map(|inputs| self.revision_keys.revision(inputs))
            .transpose()
    }

    /// Resolve an agent name (4-step chain, fail-closed):
    /// 1. A.2.1 single-segment charset guard (`invalid_name`).
    /// 2. `<dir>/<name>.toml` exists → that profile.
    /// 3. `name` is an installed runtime id → the bare base runtime.
    /// 4. else → `agent_profile_not_found` (no silent fallback).
    pub(crate) fn resolve_agent(&self, name: &str) -> Result<ResolvedAgent, ProtocolError> {
        self.resolve_agent_against(name, None)
    }

    /// [`Self::resolve_agent`] for a session recovering from a pinned
    /// definition: a profile whose base is that definition's runtime builds on
    /// `pinned` instead of whatever the registry serves now, so an unchanged
    /// profile keeps resolving after its package is disabled or another
    /// version is selected. The profile file is validated as usual.
    pub(crate) fn resolve_agent_pinned(
        &self,
        name: &str,
        pinned: &Arc<RuntimeDefinition>,
    ) -> Result<ResolvedAgent, ProtocolError> {
        self.resolve_agent_against(name, Some(pinned))
    }

    fn resolve_agent_against(
        &self,
        name: &str,
        pinned: Option<&Arc<RuntimeDefinition>>,
    ) -> Result<ResolvedAgent, ProtocolError> {
        validate_name("agent", name)?;
        if let Some(dir) = &self.dir {
            let path = dir.join(format!("{name}.toml"));
            if path.is_file() {
                return load_profile(name, &path, dir, &self.runtimes, pinned);
            }
        }
        if let Some(definition) = RuntimeId::parse(name)
            .ok()
            .and_then(|runtime_id| match pinned {
                Some(definition) if *definition.runtime_id() == runtime_id => {
                    Some(Arc::clone(definition))
                }
                _ => self.runtimes.resolve_id(&runtime_id).ok(),
            })
        {
            return Ok(ResolvedAgent {
                name: name.to_owned(),
                base: definition.runtime_id().clone(),
                definition,
                profile: None,
            });
        }
        Err(agent_profile_not_found(name))
    }

    /// Enumerate the resolvable host profiles, for `host.inspect`. Lists every
    /// `<dir>/<name>.toml` that resolves cleanly; a malformed, non-contained, or
    /// badly-named file is skipped with a warning so one bad profile never hides
    /// the rest. Sorted by name for a deterministic listing.
    pub(crate) fn enumerate(&self) -> Vec<ResolvedAgent> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let mut resolved = Vec::new();
        for (stem, _path) in profile_files(dir) {
            match self.resolve_agent(&stem) {
                Ok(agent) if agent.profile.is_some() => resolved.push(agent),
                Ok(_) => {}
                Err(err) => {
                    warn!(profile = %stem, error = %err, "skipping unresolvable agent profile during enumeration");
                }
            }
        }
        resolved.sort_by(|a, b| a.name.cmp(&b.name));
        resolved
    }

    /// The profiles that bind a package version the host cannot serve now:
    /// the digest is not installed, or its root fails verification. They are
    /// listed so `host.inspect` reports them unavailable instead of hiding
    /// them. Sorted by name.
    pub(crate) fn unavailable_pinned(&self) -> Vec<UnavailableProfile> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let mut unavailable = Vec::new();
        for (stem, path) in profile_files(dir) {
            let Ok(Some(binding)) = read_binding(dir, &stem, &path) else {
                continue;
            };
            let Some(base) = binding.base else {
                continue;
            };
            let refused = matches!(
                self.resolve_agent(&stem),
                Err(ProtocolError { ref code, .. })
                    if code == "runtime_not_installed" || code == "runtime_incompatible"
            );
            if refused {
                unavailable.push(UnavailableProfile { name: stem, base });
            }
        }
        unavailable.sort_by(|a, b| a.name.cmp(&b.name));
        unavailable
    }

    /// The package digests that host profiles pin.
    ///
    /// Every `*.toml` of the agents directory is read through the same
    /// containment and owner-security checks as a profile load, and only its
    /// `package`/`digest` keys are parsed. A file that fails a check or does
    /// not parse pins nothing and logs a warning, so one broken profile never
    /// blocks the lifecycle of another package.
    ///
    /// # Errors
    ///
    /// Returns an error when the agents directory holds more than
    /// [`MAX_SCANNED_PROFILES`] entries or cannot be listed: the scan runs
    /// under the package lifecycle authority, and an unbounded or unreadable
    /// listing must refuse the change rather than be assumed to pin nothing.
    pub(crate) fn pinned_digests(&self) -> std::io::Result<RetainedDigests> {
        let mut pinned = RetainedDigests::new();
        let Some(dir) = &self.dir else {
            return Ok(pinned);
        };
        for (stem, path) in scanned_profile_files(dir)? {
            match read_binding(dir, &stem, &path) {
                Ok(Some(binding)) => pinned.insert(binding.digest),
                Ok(None) => {}
                Err(err) => {
                    warn!(profile = %stem, error = %err, "agent profile pins no package digest");
                }
            }
        }
        Ok(pinned)
    }
}

/// A host profile whose pinned package version cannot be served right now.
#[derive(Debug, Clone)]
pub(crate) struct UnavailableProfile {
    /// The profile name.
    pub name: String,
    /// The base runtime the profile extends.
    pub base: RuntimeId,
}

/// The profile files of `dir`: `(stem, path)` of every regular `*.toml` file.
fn profile_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") || !path.is_file() {
                return None;
            }
            let stem = path.file_stem().and_then(|stem| stem.to_str())?.to_owned();
            Some((stem, path))
        })
        .collect()
}

/// Most directory entries the retention scan lists: 4096.
///
/// A host has a handful of profiles; the ceiling keeps the scan, which runs
/// under the package lifecycle authority, from walking an arbitrarily large
/// directory.
const MAX_SCANNED_PROFILES: usize = 4096;

/// The profile files of `dir` for the retention scan, listing at most
/// [`MAX_SCANNED_PROFILES`] entries.
fn scanned_profile_files(dir: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    let mut seen = 0_usize;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        seen += 1;
        if seen > MAX_SCANNED_PROFILES {
            return Err(std::io::Error::other(
                "the agents directory holds too many entries to scan",
            ));
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") || !path.is_file() {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            files.push((stem.to_owned(), path));
        }
    }
    Ok(files)
}

/// Largest profile file the retention scan reads: 1 MiB.
///
/// The scan runs while the package lifecycle authority is held, so a file that
/// is huge or endless must not stall package mutations and fresh launches. A
/// profile is a few hundred bytes of launch settings; the ceiling is far above
/// any real one.
const MAX_SCANNED_PROFILE_BYTES: u64 = 1024 * 1024;

/// Reads a profile file for the retention scan without following a final
/// symlink and without blocking on a fifo or device swapped in after the
/// listing, refusing anything that is not a regular file within
/// [`MAX_SCANNED_PROFILE_BYTES`].
fn read_bounded_profile(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let mut text = String::new();
    file.take(MAX_SCANNED_PROFILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if u64::try_from(text.len()).map_or(true, |read| read > MAX_SCANNED_PROFILE_BYTES) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    Ok(text)
}

/// The package binding keys of one profile file, parsed without the rest of
/// the profile.
#[derive(Debug, Deserialize)]
struct RawBinding {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    digest: Option<String>,
}

/// A parsed package binding of a profile file.
struct FileBinding {
    base: Option<RuntimeId>,
    digest: PackageDigest,
}

/// Read the package binding of `path`, under the containment and
/// owner-security checks of a profile load. `Ok(None)` is a profile that
/// pins nothing.
fn read_binding(dir: &Path, name: &str, path: &Path) -> Result<Option<FileBinding>, ProtocolError> {
    validate_name("agent", name)?;
    assert_contained(dir, path, name)?;
    if !file_is_owner_secure(path) {
        return Err(invalid_profile(
            name,
            "profile file is not owner-secure (wrong owner or group/world-writable)",
        ));
    }
    let content =
        read_bounded_profile(path).map_err(|err| invalid_profile(name, &err.to_string()))?;
    let raw: RawBinding = toml::from_str(&content)
        .map_err(|err| invalid_profile(name, &toml_diagnostic(&content, &err)))?;
    let Some(digest) = raw.digest else {
        return Ok(None);
    };
    let digest = PackageDigest::parse(&digest)
        .map_err(|_error| invalid_profile(name, "digest is not a valid package digest"))?;
    Ok(Some(FileBinding {
        base: raw.base.and_then(|base| RuntimeId::parse(&base).ok()),
        digest,
    }))
}

/// Whether `dir` is safe to load host config from: owned by the daemon's effective
/// user and not group/world-writable. An absent/unreadable dir is treated as secure
/// (there is simply nothing to load); only a present-but-unsafe dir fails the gate.
#[cfg(unix)]
fn dir_is_owner_secure(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(meta) = std::fs::metadata(dir) else {
        return true;
    };
    // SAFETY: `geteuid` is always safe — it reads the calling process's effective
    // uid and cannot fail.
    #[expect(unsafe_code, reason = "libc::geteuid FFI; SAFETY documented above")]
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return false;
    }
    meta.permissions().mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn dir_is_owner_secure(_dir: &Path) -> bool {
    true
}

/// Whether a host-authored config file is owned by the daemon's effective user
/// and not group/world-writable. Missing/unreadable files fail closed because a
/// caller is about to read this exact path.
#[cfg(unix)]
fn file_is_owner_secure(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    // SAFETY: `geteuid` is always safe — it reads the calling process's effective
    // uid and cannot fail.
    #[expect(unsafe_code, reason = "libc::geteuid FFI; SAFETY documented above")]
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return false;
    }
    meta.permissions().mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn file_is_owner_secure(_path: &Path) -> bool {
    true
}

/// Canonicalize `candidate` and assert it stays within the canonicalized `base_dir`
/// tree — owner-checking a file alone is insufficient because a symlink would exec
/// its (out-of-tree) target. Returns the canonical path on success.
fn assert_contained(base_dir: &Path, candidate: &Path, name: &str) -> Result<(), ProtocolError> {
    let canon_base = std::fs::canonicalize(base_dir)
        .map_err(|err| invalid_profile(name, &format!("agents directory: {err}")))?;
    let canon = std::fs::canonicalize(candidate)
        .map_err(|err| invalid_profile(name, &format!("{}: {err}", candidate.display())))?;
    if !canon.starts_with(&canon_base) {
        return Err(invalid_profile(
            name,
            "resolves outside the agents directory (symlink escape)",
        ));
    }
    Ok(())
}

fn load_profile(
    name: &str,
    path: &Path,
    dir: &Path,
    runtimes: &RuntimeHost,
    pinned: Option<&Arc<RuntimeDefinition>>,
) -> Result<ResolvedAgent, ProtocolError> {
    // Containment first: a symlinked `<name>.toml` that escapes the tree must be
    // rejected before its contents are read or exec'd (C.5).
    assert_contained(dir, path, name)?;
    if !file_is_owner_secure(path) {
        return Err(invalid_profile(
            name,
            "profile file is not owner-secure (wrong owner or group/world-writable)",
        ));
    }
    let content =
        std::fs::read_to_string(path).map_err(|err| invalid_profile(name, &err.to_string()))?;
    let raw: RawProfile = toml::from_str(&content)
        .map_err(|err| invalid_profile(name, &toml_diagnostic(&content, &err)))?;
    let base_id = RuntimeId::parse(&raw.base)
        .map_err(|_error| invalid_profile(name, &format!("unknown base kind '{}'", raw.base)))?;
    let pin = parse_package_pin(name, raw.package.as_deref(), raw.digest.as_deref())?;
    // A session recovering from a pinned definition keeps it: the profile's own
    // binding applies to fresh launches.
    let definition = match pinned {
        Some(definition) if *definition.runtime_id() == base_id => Arc::clone(definition),
        _ => resolve_base(name, runtimes, &base_id, pin.as_ref())?,
    };
    // A runtime without native resume (the shell) cannot have a profile claim one.
    if definition.native().is_none()
        && raw
            .resume
            .as_ref()
            .is_some_and(|resume| resume.resumable != Some(false))
    {
        return Err(invalid_profile(
            name,
            &format!(
                "base = \"{}\" cannot declare a [resume] spec (the runtime has no native resume)",
                raw.base
            ),
        ));
    }
    let program = raw
        .program
        .unwrap_or_else(|| definition.program().as_str().to_owned());
    let args = raw.args.unwrap_or_default();
    // Every `POHUNEK_`-prefixed key is reserved for the daemon handshake; strip the
    // whole prefix so a profile can never shadow `POHUNEK_ENV`/`_PROTOCOL_VERSION`/…
    // (the launch path also re-asserts this by appending the handshake env last).
    let env: Vec<(String, String)> = raw
        .env
        .into_iter()
        .filter(|(key, _)| !key.starts_with("POHUNEK_"))
        .map(|(key, value)| (key, value.0))
        .collect();
    let input_rules = raw.input_rules.map(|rules| {
        definition.input_rules().with_framing(
            rules.bracketed_paste.unwrap_or(false),
            Duration::from_millis(rules.submit_delay_ms.unwrap_or(0)),
        )
    });
    let native = resolve_native(name, &definition, raw.resume.as_ref())?;
    let manifest_source = resolve_manifest(name, dir, raw.manifest.as_deref())?;
    let binding_json = serde_json::to_vec(definition.binding())
        .map_err(|err| invalid_profile(name, &format!("launch binding: {err}")))?;
    let inputs = ProfileInputs::of(
        &content,
        manifest_source.as_ref().map(|source| source.text.as_str()),
        &binding_json,
        &program,
        &args,
    )?;
    Ok(ResolvedAgent {
        name: name.to_owned(),
        base: base_id,
        definition,
        profile: Some(ResolvedProfile {
            program,
            args,
            env,
            input_rules,
            native,
            manifest: manifest_source.map(|source| source.manifest),
            inputs,
        }),
    })
}

/// The package binding of a profile: the package id and archive digest its
/// base runtime is launched from.
#[derive(Debug, Clone)]
struct PackagePin {
    package: PackageId,
    digest: PackageDigest,
}

/// Parse the `package`/`digest` keys: both or neither.
fn parse_package_pin(
    name: &str,
    package: Option<&str>,
    digest: Option<&str>,
) -> Result<Option<PackagePin>, ProtocolError> {
    let (package, digest) = match (package, digest) {
        (None, None) => return Ok(None),
        (Some(package), Some(digest)) => (package, digest),
        _ => {
            return Err(invalid_profile(
                name,
                "package and digest must be set together",
            ))
        }
    };
    let package = PackageId::parse(package)
        .map_err(|_error| invalid_profile(name, "package is not a valid package id"))?;
    let digest = PackageDigest::parse(digest).map_err(|_error| {
        invalid_profile(
            name,
            "digest must be sha256: followed by 64 lowercase hex digits",
        )
    })?;
    Ok(Some(PackagePin { package, digest }))
}

/// Resolve the base runtime of a profile under its package binding.
///
/// A built-in base takes no binding. A package-served base requires one and is
/// then resolved from exactly the bound digest; an unbound profile naming a
/// package-served base is refused with the migration command.
fn resolve_base(
    name: &str,
    runtimes: &RuntimeHost,
    base: &RuntimeId,
    pin: Option<&PackagePin>,
) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
    match (pin, runtimes.served_by(base)) {
        (Some(_), Some(ServedBy::Builtin)) => Err(invalid_profile(
            name,
            &format!("package and digest apply only to a package-served base, and '{base}' is built in"),
        )),
        (Some(pin), _) => runtimes.resolve_profile_pin(base, &pin.package, &pin.digest),
        (None, Some(ServedBy::Package(package))) => Err(invalid_profile(
            name,
            &format!(
                "base '{base}' is served by the installed package '{package}' and the profile does not pin it; run `pohunek plugin profile migrate {name}`"
            ),
        )),
        (None, _) => runtimes.resolve_id(base),
    }
}

/// Resolve a profile's effective native-session launch spec.
///
/// An absent `[resume]` inherits the base kind's compiled spec. `resumable =
/// false` yields no native recovery. Otherwise `[resume]` must carry a complete
/// spec (`reference_kind` and `args`, optionally `fork_args` on a base with
/// compiled fork support); nothing is merged
/// with the base kind, and every template is validated here so a malformed
/// profile fails before any session is created.
fn resolve_native(
    name: &str,
    definition: &RuntimeDefinition,
    raw: Option<&RawResume>,
) -> Result<Option<NativeSessionLaunch>, ProtocolError> {
    let Some(raw) = raw else {
        return Ok(definition.native().cloned());
    };
    if raw.resumable == Some(false) {
        if raw.reference_kind.is_some() || raw.args.is_some() || raw.fork_args.is_some() {
            return Err(invalid_profile(
                name,
                "resume.resumable = false cannot be combined with resume.reference_kind, resume.args, or resume.fork_args",
            ));
        }
        return Ok(None);
    }
    // The launch template and existence check of an assigned reference belong
    // to the runtime; a profile restating `[resume]` would drop them.
    if definition
        .native()
        .is_some_and(|native| native.assigned().is_some())
    {
        return Err(invalid_profile(
            name,
            "a base runtime that assigns its native reference cannot override [resume]",
        ));
    }
    let reference_kind = raw
        .reference_kind
        .as_deref()
        .ok_or_else(|| invalid_profile(name, "resume.reference_kind is required"))
        .and_then(|value| parse_reference_kind(name, value))?;
    let resume_args = raw
        .args
        .as_deref()
        .ok_or_else(|| invalid_profile(name, "resume.args is required"))
        .and_then(|tokens| parse_template(name, "resume.args", tokens))?;
    // Fork semantics are owned by the compiled base: a profile may restate or
    // omit its base's fork, but cannot grant fork to a base that has none.
    if raw.fork_args.is_some()
        && !definition
            .native()
            .is_some_and(NativeSessionLaunch::supports_fork)
    {
        return Err(invalid_profile(
            name,
            "resume.fork_args requires a base kind with native fork support",
        ));
    }
    let fork_args = raw
        .fork_args
        .as_deref()
        .map(|tokens| parse_template(name, "resume.fork_args", tokens))
        .transpose()?;
    Ok(Some(NativeSessionLaunch::new(
        reference_kind,
        resume_args,
        fork_args,
    )))
}

fn parse_template(name: &str, field: &str, tokens: &[String]) -> Result<NativeArgs, ProtocolError> {
    NativeArgs::from_template(tokens)
        .map_err(|err| invalid_profile(name, &format!("{field}: {err}")))
}

fn parse_reference_kind(name: &str, value: &str) -> Result<SessionRefKind, ProtocolError> {
    match value {
        "id" => Ok(SessionRefKind::Id),
        "path" => Ok(SessionRefKind::Path),
        other => Err(invalid_profile(
            name,
            &format!("unknown resume.reference_kind '{other}' (expected 'id' or 'path')"),
        )),
    }
}

/// Describe a TOML parse failure by message and line only.
///
/// The error's own `Display` quotes the offending source line, which can be an
/// `[env]` entry carrying a secret; the message and line number never do.
fn toml_diagnostic(content: &str, error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => {
            let line = content
                .get(..span.start)
                .map_or(1, |head| head.matches('\n').count() + 1);
            format!("line {line}: {}", error.message())
        }
        None => error.message().to_owned(),
    }
}

/// Resolve a profile's optional detection-manifest override (C.3) from
/// `<dir>/manifests/<manifest>.toml`, under the same charset + containment guard as
/// a profile file. A malformed manifest fails the profile closed (`invalid_profile`)
/// rather than panicking the daemon; an empty-rule manifest is accepted (it parses
/// fine and simply disables detection).
fn resolve_manifest(
    name: &str,
    dir: &Path,
    manifest_name: Option<&str>,
) -> Result<Option<ManifestSource>, ProtocolError> {
    let Some(manifest_name) = manifest_name else {
        return Ok(None);
    };
    validate_name("manifest", manifest_name)?;
    let path = dir.join("manifests").join(format!("{manifest_name}.toml"));
    if !path.is_file() {
        return Err(invalid_profile(
            name,
            &format!("manifest '{manifest_name}' not found in agents/manifests"),
        ));
    }
    assert_contained(dir, &path, name)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|err| invalid_profile(name, &format!("manifest '{manifest_name}': {err}")))?;
    let manifest = Manifest::parse_str(&content)
        .map_err(|err| invalid_profile(name, &format!("manifest '{manifest_name}': {err}")))?;
    Ok(Some(ManifestSource {
        manifest,
        text: content,
    }))
}

/// A parsed override manifest with the file text it was parsed from.
struct ManifestSource {
    manifest: Manifest,
    text: String,
}

/// `runtime/agent_profile_not_found`: a name resolved to neither a host profile nor
/// a base kind (fail-closed; no silent default).
pub(crate) fn agent_profile_not_found(name: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_profile_not_found",
        format!("no agent profile or base kind named '{name}' on this host"),
        Some(
            "use shell|codex|claude|hermes, or add ~/.config/pohunek/agents/<name>.toml on the target host"
                .to_owned(),
        ),
    )
}

/// `runtime/invalid_profile`: a profile file failed to parse, named an unknown base
/// kind, or violated a load-time rule (e.g. shell + resumable).
fn invalid_profile(name: &str, reason: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "invalid_profile",
        format!("invalid agent profile '{name}': {reason}"),
        None,
    )
}

#[cfg(test)]
mod pin_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{NativeArg, SessionRef};

    fn tmp_agents_dir(tag: &str) -> crate::test_support::ScopedDir {
        crate::test_support::scoped_dir(&format!("pohunek-agents-{tag}-"))
    }

    #[test]
    fn bare_base_kinds_resolve_without_a_profile() {
        let reg = ProfileRegistry::new(None);
        for (name, base) in [
            ("shell", RuntimeId::shell()),
            ("codex", RuntimeId::codex()),
            ("claude", RuntimeId::claude()),
            ("hermes", RuntimeId::hermes()),
        ] {
            let resolved = reg.resolve_agent(name).expect("base kind resolves");
            assert_eq!(resolved.base, base);
            assert_eq!(resolved.name, name);
            assert!(
                resolved.profile.is_none(),
                "a bare base kind has no profile override"
            );
        }
    }

    #[test]
    fn unknown_name_is_agent_profile_not_found() {
        let dir = tmp_agents_dir("missing");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let err = reg.resolve_agent("nope").expect_err("no such agent");
        assert_eq!(err.code, "agent_profile_not_found");
    }

    #[test]
    fn bad_names_are_invalid_name() {
        let dir = tmp_agents_dir("bad");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        for bad in ["../etc", "a/b", "a\\b", "-x", ".hidden", "", "a\u{7}b"] {
            let err = reg.resolve_agent(bad).expect_err("must reject");
            assert_eq!(err.code, "invalid_name", "name {bad:?}");
        }
    }

    #[test]
    fn profile_overrides_program_args_env_input_rules() {
        let dir = tmp_agents_dir("override");
        std::fs::write(
            dir.join("claude-sonnet.toml"),
            "base = \"claude\"\n\
             program = \"claude\"\n\
             args = [\"--model\", \"claude-sonnet-4\"]\n\
             [env]\n\
             ANTHROPIC_MODEL = \"claude-sonnet-4\"\n\
             POHUNEK_ENV = \"0\"\n\
             [input_rules]\n\
             bracketed_paste = false\n\
             submit_delay_ms = 150\n",
        )
        .expect("write profile");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let resolved = reg.resolve_agent("claude-sonnet").expect("resolves");
        assert_eq!(resolved.base, RuntimeId::claude());
        assert_eq!(resolved.name, "claude-sonnet");
        let profile = resolved.profile.expect("has overrides");
        assert_eq!(profile.program, "claude");
        assert_eq!(profile.args, vec!["--model", "claude-sonnet-4"]);
        // ANTHROPIC_MODEL is kept; the reserved POHUNEK_ key is stripped.
        assert!(profile
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "claude-sonnet-4"));
        assert!(
            !profile.env.iter().any(|(k, _)| k.starts_with("POHUNEK_")),
            "POHUNEK_-prefixed profile env must be stripped: {:?}",
            profile.env
        );
        let rules = profile.input_rules.expect("input rules override");
        assert!(!rules.bracketed_paste);
        assert_eq!(rules.submit_delay, Duration::from_millis(150));
    }

    #[test]
    fn shell_base_with_resumable_is_rejected_at_load() {
        let dir = tmp_agents_dir("shell-resume");
        std::fs::write(
            dir.join("myshell.toml"),
            "base = \"shell\"\n[resume]\nresumable = true\n",
        )
        .expect("write profile");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let err = reg
            .resolve_agent("myshell")
            .expect_err("shell+resumable rejected");
        assert_eq!(err.code, "invalid_profile");
    }

    #[test]
    fn hermes_profile_resolves_program_args_resume_and_no_fork() {
        let dir = tmp_agents_dir("hermes-profile");
        write_profile(
            &dir,
            "hermes-work",
            "base = \"hermes\"\nprogram = \"hermes-wrapper\"\nargs = [\"-p\", \"work\", \"chat\"]\n[input_rules]\nbracketed_paste = false\nsubmit_delay_ms = 25\n",
        );
        let resolved = ProfileRegistry::new(Some(dir.clone()))
            .resolve_agent("hermes-work")
            .expect("Hermes profile resolves");

        assert_eq!(resolved.base, RuntimeId::hermes());
        let profile = resolved.profile.expect("profile overrides");
        assert_eq!(profile.program, "hermes-wrapper");
        assert_eq!(profile.args, vec!["-p", "work", "chat"]);
        let input_rules = profile.input_rules.expect("framing override");
        assert!(!input_rules.bracketed_paste);
        assert_eq!(input_rules.submit_delay, Duration::from_millis(25));
        assert!(input_rules.validate_text("unsafe\u{1b}[201~").is_err());
        assert!(!input_rules.allows_while_blocked());
        assert_eq!(
            profile.native,
            crate::agent::builtin_native_launch(&protocol::RuntimeId::hermes())
        );
        assert!(!profile.native.expect("Hermes recovers").supports_fork());
    }

    #[test]
    fn an_uninstalled_base_runtime_is_runtime_not_installed() {
        let dir = tmp_agents_dir("bad-base");
        std::fs::write(dir.join("weird.toml"), "base = \"emacs\"\n").expect("write profile");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let err = reg
            .resolve_agent("weird")
            .expect_err("uninstalled base rejected");
        assert_eq!(err.code, "runtime_not_installed");
    }

    #[test]
    fn a_base_that_is_not_a_runtime_id_is_invalid_profile() {
        let dir = tmp_agents_dir("bad-base-grammar");
        for (name, base) in [("upper", "Emacs"), ("spaced", "my agent"), ("empty", "")] {
            std::fs::write(
                dir.join(format!("{name}.toml")),
                format!("base = \"{base}\"\n"),
            )
            .expect("write profile");
            let err = ProfileRegistry::new(Some(dir.clone()))
                .resolve_agent(name)
                .expect_err("grammar-invalid base rejected");
            assert_eq!(err.code, "invalid_profile", "{base:?}");
        }
    }

    #[test]
    fn unknown_profile_key_is_invalid_profile() {
        let dir = tmp_agents_dir("bad-key");
        std::fs::write(
            dir.join("p.toml"),
            "base = \"claude\"\nflags = [\"--danger\"]\n",
        )
        .expect("write profile");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let err = reg.resolve_agent("p").expect_err("unknown key rejected");
        assert_eq!(err.code, "invalid_profile");
    }

    #[cfg(unix)]
    #[test]
    fn group_writable_profile_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp_agents_dir("bad-file-mode");
        let path = dir.join("unsafe.toml");
        std::fs::write(&path, "base = \"claude\"\nprogram = \"/bin/sh\"\n").expect("write profile");
        let mut perms = std::fs::metadata(&path)
            .expect("profile metadata")
            .permissions();
        perms.set_mode(0o660);
        std::fs::set_permissions(&path, perms).expect("set profile mode");

        let reg = ProfileRegistry::new(Some(dir.clone()));
        let err = reg
            .resolve_agent("unsafe")
            .expect_err("group-writable profile file rejected");
        assert_eq!(err.code, "invalid_profile");
    }

    /// A non-empty, valid override manifest (mirrors the detect fixture).
    const VALID_MANIFEST: &str = "[[rules]]\n\
         id = \"custom-blocked\"\n\
         state = \"blocked\"\n\
         priority = 1\n\
         region = \"whole_recent\"\n\
         contains = \"custom blocker\"\n";

    fn write_profile(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(format!("{name}.toml")), body).expect("write profile");
    }

    fn write_manifest(dir: &Path, name: &str, body: &str) {
        let manifests = dir.join("manifests");
        std::fs::create_dir_all(&manifests).expect("create manifests dir");
        std::fs::write(manifests.join(format!("{name}.toml")), body).expect("write manifest");
    }

    fn resolve(dir: &Path, name: &str) -> Result<ResolvedProfile, ProtocolError> {
        ProfileRegistry::new(Some(dir.to_path_buf()))
            .resolve_agent(name)
            .map(|agent| agent.profile.expect("profile overrides"))
    }

    fn invalid_message(dir: &Path, name: &str) -> String {
        let error = resolve(dir, name).expect_err("profile must be rejected");
        assert_eq!(error.code, "invalid_profile");
        error.msg
    }

    #[test]
    fn native_launch_inherits_base_kind_without_a_resume_block() {
        let dir = tmp_agents_dir("resume-inherit");
        write_profile(&dir, "c", "base = \"claude\"\n");
        write_profile(&dir, "x", "base = \"codex\"\n");
        write_profile(&dir, "h", "base = \"hermes\"\n");

        for (name, base) in [
            ("c", RuntimeId::claude()),
            ("x", RuntimeId::codex()),
            ("h", RuntimeId::hermes()),
        ] {
            let agent = ProfileRegistry::new(Some(dir.clone()))
                .resolve_agent(name)
                .expect("resolves");
            assert_eq!(
                agent.native_launch(),
                crate::agent::builtin_native_launch(&base),
                "{name}"
            );
            assert_eq!(
                agent.profile.expect("profile").native,
                crate::agent::builtin_native_launch(&base)
            );
        }
    }

    #[test]
    fn resume_only_spec_resolves_without_fork() {
        let dir = tmp_agents_dir("resume-only");
        write_profile(
            &dir,
            "weird",
            "base = \"claude\"\n[resume]\nreference_kind = \"path\"\nargs = [\"resume\", \"{reference}\"]\n",
        );
        let native = resolve(&dir, "weird")
            .expect("resolves")
            .native
            .expect("spec");
        assert_eq!(native.reference_kind(), SessionRefKind::Path);
        assert_eq!(
            native.resume_args().as_slice(),
            [
                NativeArg::Literal("resume".to_owned()),
                NativeArg::Reference
            ]
        );
        assert!(!native.supports_fork(), "fork is explicit, never inherited");
    }

    #[test]
    fn pi_shaped_spec_resolves_resume_and_fork() {
        let dir = tmp_agents_dir("pi-shaped");
        write_profile(
            &dir,
            "pi-like",
            "base = \"claude\"\nprogram = \"pi\"\n[resume]\nreference_kind = \"path\"\nargs = [\"--session\", \"{reference}\"]\nfork_args = [\"--fork\", \"{reference}\"]\n",
        );
        let native = resolve(&dir, "pi-like")
            .expect("resolves")
            .native
            .expect("spec");
        let path = SessionRef::path("/work/a b/$(x);.jsonl").expect("path reference");
        assert_eq!(native.reference_kind(), SessionRefKind::Path);
        assert_eq!(
            native.resume_argv(&path).expect("resume"),
            vec!["--session", "/work/a b/$(x);.jsonl"]
        );
        assert_eq!(
            native.fork_argv(&path).expect("fork"),
            vec!["--fork", "/work/a b/$(x);.jsonl"]
        );
    }

    #[test]
    fn spec_may_restate_the_claude_shapes_with_or_without_fork() {
        let dir = tmp_agents_dir("claude-restated");
        write_profile(
            &dir,
            "with-fork",
            "base = \"claude\"\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]\nfork_args = [\"--resume\", \"{reference}\", \"--fork-session\"]\n",
        );
        write_profile(
            &dir,
            "no-fork",
            "base = \"claude\"\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]\n",
        );
        assert_eq!(
            resolve(&dir, "with-fork").expect("resolves").native,
            crate::agent::builtin_native_launch(&protocol::RuntimeId::claude())
        );
        let no_fork = resolve(&dir, "no-fork")
            .expect("resolves")
            .native
            .expect("spec");
        assert!(!no_fork.supports_fork());
    }

    #[test]
    fn resumable_false_yields_no_native_launch() {
        let dir = tmp_agents_dir("resume-off");
        write_profile(
            &dir,
            "noresume",
            "base = \"claude\"\n[resume]\nresumable = false\n",
        );
        let agent = ProfileRegistry::new(Some(dir.clone()))
            .resolve_agent("noresume")
            .expect("resolves");
        assert_eq!(agent.native_launch(), None);
        assert_eq!(
            agent.profile.expect("profile").native,
            None,
            "resumable=false is authoritative, not base-fallback"
        );
    }

    #[test]
    fn malformed_resume_specs_are_rejected_with_profile_and_field() {
        let dir = tmp_agents_dir("resume-bad-specs");
        let cases: [(&str, &str, &str); 14] = [
            (
                "unknown-kind",
                "reference_kind = \"socket\"\nargs = [\"--resume\", \"{reference}\"]\n",
                "unknown resume.reference_kind",
            ),
            (
                "missing-kind",
                "args = [\"--resume\", \"{reference}\"]\n",
                "resume.reference_kind is required",
            ),
            (
                "missing-args",
                "reference_kind = \"id\"\n",
                "resume.args is required",
            ),
            (
                "empty-table",
                "",
                "resume.reference_kind is required",
            ),
            (
                "empty-args",
                "reference_kind = \"id\"\nargs = []\n",
                "resume.args: the argument list is empty",
            ),
            (
                "no-sentinel",
                "reference_kind = \"id\"\nargs = [\"--resume\"]\n",
                "resume.args: the argument list has no `{reference}` placeholder",
            ),
            (
                "duplicate-sentinel",
                "reference_kind = \"id\"\nargs = [\"{reference}\", \"{reference}\"]\n",
                "resume.args: the argument list has more than one",
            ),
            (
                "embedded",
                "reference_kind = \"id\"\nargs = [\"--session={reference}\"]\n",
                "resume.args: argument 0 contains braces",
            ),
            (
                "unknown-placeholder",
                "reference_kind = \"id\"\nargs = [\"--resume\", \"{ref}\", \"{reference}\"]\n",
                "resume.args: argument 1 contains braces",
            ),
            (
                "empty-literal",
                "reference_kind = \"id\"\nargs = [\"\", \"{reference}\"]\n",
                "resume.args: argument 0 is an empty literal",
            ),
            (
                "bad-fork",
                "reference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]\nfork_args = [\"--fork\"]\n",
                "resume.fork_args: the argument list has no `{reference}` placeholder",
            ),
            (
                "empty-fork",
                "reference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]\nfork_args = []\n",
                "resume.fork_args: the argument list is empty",
            ),
            (
                "fork-without-resume",
                "reference_kind = \"id\"\nfork_args = [\"--fork\", \"{reference}\"]\n",
                "resume.args is required",
            ),
            (
                "off-with-spec",
                "resumable = false\nreference_kind = \"id\"\n",
                "cannot be combined",
            ),
        ];
        for (name, body, expected) in cases {
            write_profile(&dir, name, &format!("base = \"claude\"\n[resume]\n{body}"));
            let message = invalid_message(&dir, name);
            assert!(message.contains(&format!("'{name}'")), "{name}: {message}");
            assert!(message.contains(expected), "{name}: {message}");
        }
    }

    #[test]
    fn removed_mode_and_fork_table_keys_are_rejected() {
        let dir = tmp_agents_dir("resume-removed-keys");
        write_profile(
            &dir,
            "mode",
            "base = \"claude\"\n[resume]\nmode = \"flag\"\n",
        );
        write_profile(
            &dir,
            "ref-kind",
            "base = \"claude\"\n[resume]\nref_kind = \"id\"\n",
        );
        write_profile(
            &dir,
            "fork-table",
            "base = \"claude\"\n[fork]\nsupported = false\n",
        );
        for name in ["mode", "ref-kind", "fork-table"] {
            assert!(
                invalid_message(&dir, name).contains("unknown field"),
                "{name}"
            );
        }
    }

    #[test]
    fn fork_args_require_a_base_with_compiled_fork_support() {
        let dir = tmp_agents_dir("fork-args-base");
        let spec = "[resume]\nreference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]\nfork_args = [\"--fork\", \"{reference}\"]\n";
        for base in ["codex", "hermes"] {
            let name = format!("{base}-fork");
            write_profile(&dir, &name, &format!("base = \"{base}\"\n{spec}"));
            let message = invalid_message(&dir, &name);
            assert!(message.contains(&format!("'{name}'")), "{message}");
            assert!(
                message.contains("resume.fork_args requires a base kind with native fork support"),
                "{message}"
            );
        }
        write_profile(&dir, "claude-fork", &format!("base = \"claude\"\n{spec}"));
        let native = resolve(&dir, "claude-fork")
            .expect("claude owns fork")
            .native;
        assert!(native.expect("spec").supports_fork());
    }

    #[test]
    fn shell_base_cannot_declare_a_resume_spec() {
        let dir = tmp_agents_dir("shell-spec");
        write_profile(
            &dir,
            "sh-spec",
            "base = \"shell\"\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]\n",
        );
        assert!(invalid_message(&dir, "sh-spec").contains("shell"));
    }

    #[test]
    fn diagnostics_never_echo_profile_env_values() {
        let dir = tmp_agents_dir("env-leak");
        let secret = "s3cr3t-sentinel-value";
        write_profile(
            &dir,
            "dup-env",
            &format!("base = \"claude\"\n[env]\nTOKEN = \"{secret}\"\nTOKEN = \"{secret}-2\"\n"),
        );
        write_profile(
            &dir,
            "typed-env",
            "base = \"claude\"\n[env]\nPIN = 123456\n",
        );
        write_profile(
            &dir,
            "bad-template",
            &format!(
                "base = \"claude\"\n[env]\nTOKEN = \"{secret}\"\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume\"]\n"
            ),
        );
        for name in ["dup-env", "typed-env", "bad-template"] {
            let message = invalid_message(&dir, name);
            assert!(!message.contains(secret), "{name}: {message}");
            assert!(!message.contains("123456"), "{name}: {message}");
        }
        assert!(invalid_message(&dir, "typed-env").contains("environment values must be strings"));
    }

    #[test]
    fn manifest_override_resolves_and_parses() {
        let dir = tmp_agents_dir("manifest-ok");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"mine\"\n");
        write_manifest(&dir, "mine", VALID_MANIFEST);
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let manifest = reg
            .resolve_agent("p")
            .expect("resolves")
            .profile
            .unwrap()
            .manifest;
        assert!(
            manifest.is_some(),
            "the override manifest must be parsed and carried"
        );
    }

    #[test]
    fn empty_rule_manifest_is_accepted_with_detection_disabled() {
        // The documented C.3 decision: an empty manifest parses fine (no rules) and
        // is accepted, NOT treated as a load error.
        let dir = tmp_agents_dir("manifest-empty");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"empty\"\n");
        write_manifest(&dir, "empty", "");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let manifest = reg
            .resolve_agent("p")
            .expect("resolves")
            .profile
            .unwrap()
            .manifest;
        assert!(
            manifest.is_some(),
            "an empty-rule manifest loads (detection disabled)"
        );
    }

    #[test]
    fn malformed_manifest_disables_only_that_profile() {
        let dir = tmp_agents_dir("manifest-bad");
        write_profile(&dir, "broken", "base = \"codex\"\nmanifest = \"bad\"\n");
        // A typed ManifestError (invalid state), not just bad TOML.
        write_manifest(
            &dir,
            "bad",
            "[[rules]]\nid = \"x\"\nstate = \"telepathic\"\npriority = 1\nregion = \"whole_recent\"\ncontains = \"x\"\n",
        );
        // A second, healthy profile must still resolve — one bad manifest does not
        // poison the registry, and the daemon does not panic.
        write_profile(&dir, "good", "base = \"claude\"\n");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        assert_eq!(
            reg.resolve_agent("broken")
                .expect_err("malformed manifest")
                .code,
            "invalid_profile"
        );
        assert!(
            reg.resolve_agent("good").is_ok(),
            "other profiles still load"
        );
    }

    #[test]
    fn missing_or_badly_named_manifest_is_rejected() {
        let dir = tmp_agents_dir("manifest-missing");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"ghost\"\n");
        write_profile(&dir, "q", "base = \"codex\"\nmanifest = \"../escape\"\n");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        assert_eq!(
            reg.resolve_agent("p").expect_err("missing manifest").code,
            "invalid_profile"
        );
        // A traversal-shaped manifest name is rejected by the A.2.1 charset guard.
        assert_eq!(
            reg.resolve_agent("q").expect_err("bad manifest name").code,
            "invalid_name"
        );
    }

    #[test]
    fn enumerate_lists_resolvable_profiles_and_skips_bad_ones() {
        let dir = tmp_agents_dir("enumerate");
        write_profile(&dir, "alpha", "base = \"claude\"\n");
        write_profile(&dir, "beta", "base = \"codex\"\n");
        write_profile(&dir, "broken", "base = \"emacs\"\n"); // unknown base → skipped
        let reg = ProfileRegistry::new(Some(dir.clone()));
        let names: Vec<String> = reg.enumerate().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_writable_agents_dir_loads_no_profiles() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_agents_dir("insecure-agents");
        write_profile(&dir, "p", "base = \"claude\"\n");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
            .expect("chmod world-writable");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        // The whole host layer is disabled: the profile name is now unresolvable.
        assert_eq!(
            reg.resolve_agent("p")
                .expect_err("insecure dir disables profiles")
                .code,
            "agent_profile_not_found"
        );
        assert!(reg.enumerate().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_writable_manifests_dir_loads_no_profiles() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_agents_dir("insecure-manifests");
        write_profile(&dir, "p", "base = \"claude\"\n");
        write_manifest(&dir, "m", VALID_MANIFEST);
        std::fs::set_permissions(
            dir.join("manifests"),
            std::fs::Permissions::from_mode(0o777),
        )
        .expect("chmod world-writable manifests");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        // An untrusted manifests/ dir fails the whole tree closed.
        assert_eq!(
            reg.resolve_agent("p")
                .expect_err("insecure manifests disables profiles")
                .code,
            "agent_profile_not_found"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_profile_escaping_the_tree_is_rejected() {
        let dir = tmp_agents_dir("symlink-profile");
        let outside = tmp_agents_dir("symlink-outside");
        std::fs::write(outside.join("evil.toml"), "base = \"claude\"\n").expect("write outside");
        std::os::unix::fs::symlink(outside.join("evil.toml"), dir.join("evil.toml"))
            .expect("symlink into tree");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        assert_eq!(
            reg.resolve_agent("evil").expect_err("symlink escape").code,
            "invalid_profile"
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_manifest_resolving_outside_the_tree_is_rejected() {
        let dir = tmp_agents_dir("symlink-manifest");
        let outside = tmp_agents_dir("symlink-manifest-outside");
        std::fs::write(outside.join("evil.toml"), VALID_MANIFEST).expect("write outside manifest");
        let manifests = dir.join("manifests");
        std::fs::create_dir_all(&manifests).expect("create manifests dir");
        std::os::unix::fs::symlink(outside.join("evil.toml"), manifests.join("m.toml"))
            .expect("symlink manifest into tree");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"m\"\n");
        let reg = ProfileRegistry::new(Some(dir.clone()));
        assert_eq!(
            reg.resolve_agent("p")
                .expect_err("manifest symlink escape")
                .code,
            "invalid_profile"
        );
    }

    fn assigned_registry(dir: &std::path::Path) -> ProfileRegistry {
        ProfileRegistry::with_runtimes(
            Some(dir.to_path_buf()),
            crate::agent::host::fixture::pi_shaped_host(
                std::path::Path::new("/bin/sh"),
                crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
            ),
        )
    }

    /// A profile body on the Pi-shaped package base: its package binding, then `rest`.
    fn pinned_pi(rest: &str) -> String {
        format!(
            "base = \"pi\"\n{}{rest}",
            crate::agent::host::fixture::pi_shaped_profile_pin()
        )
    }

    #[test]
    fn a_profile_inherits_the_assignment_of_its_base_runtime() {
        let dir = tmp_agents_dir("assigned-inherit");
        write_profile(&dir, "mine", &pinned_pi("args = [\"--x\"]\n"));
        let agent = assigned_registry(&dir)
            .resolve_agent("mine")
            .expect("resolves");
        let native = agent.native_launch().expect("recovers");
        assert!(native.assigned().is_some());
        assert_eq!(native, agent.definition.native().cloned().expect("base"));
    }

    #[test]
    fn a_profile_cannot_override_resume_on_an_assigned_base() {
        let dir = tmp_agents_dir("assigned-override");
        write_profile(
            &dir,
            "restated",
            &pinned_pi(
                "[resume]\nreference_kind = \"id\"\nargs = [\"--session\", \"{reference}\"]\n",
            ),
        );
        let error = assigned_registry(&dir)
            .resolve_agent("restated")
            .expect_err("override rejected");
        assert_eq!(error.code, "invalid_profile");
    }

    #[test]
    fn a_profile_may_switch_recovery_off_on_an_assigned_base() {
        let dir = tmp_agents_dir("assigned-off");
        write_profile(&dir, "off", &pinned_pi("[resume]\nresumable = false\n"));
        let agent = assigned_registry(&dir)
            .resolve_agent("off")
            .expect("resolves");
        assert_eq!(agent.native_launch(), None);
    }
}
