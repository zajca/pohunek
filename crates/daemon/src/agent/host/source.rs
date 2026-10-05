//! Where a launch request gets its agent from: the owner's host or a relay.
//!
//! A [`LaunchSource`] is the only way a launch names the agent to run. The
//! owner-local source resolves a name on this host (a host profile, else an
//! installed runtime id). The relay source resolves ONLY a profile the owner
//! has locally approved, identified by its name and the [`ProfileRevision`]
//! that was approved: it can never name a runtime id, package, program or
//! argv, and an edited profile fails with `agent_profile_revision_stale`
//! instead of launching something the owner did not approve. A revision is a
//! MAC keyed by a host-local secret, so it never lets a holder confirm a
//! guessed `[env]` value.
//!
//! Wiring a relay-origin `session.new` to [`LaunchSource::RelayProfile`]
//! depends on the locally approved `HostShare` of issue #82; until then the
//! relay variant is exercised at this seam only.

// Rust guideline compliant 2026-10-05

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac as _};
use protocol::{ErrorClass, ProtocolError};
use sha2::{Digest, Sha256};
use tracing::warn;
use zeroize::Zeroizing;

use crate::agent::profile::agent_profile_not_found;
use crate::agent::{ProfileRegistry, ResolvedAgent};
use crate::host_state::HostStateDir;

/// Domain separator of the input digest; a different input shape would need a
/// different tag so digests never collide across shapes.
const INPUTS_DOMAIN: &[u8] = b"pohunek.agent-profile-inputs";

/// Domain separator of the revision MAC.
const REVISION_DOMAIN: &[u8] = b"pohunek.agent-profile-revision";

/// Domain separator of the config-home identifier MAC; the same key under a
/// different domain never yields a value usable as a profile revision.
const CONFIG_HOME_ID_DOMAIN: &[u8] = b"pohunek.config-home-id";

/// Bytes of a config-home identifier before hex encoding.
///
/// 128 bits keep accidental collisions between the few homes of one host
/// negligible and keep the identifier short enough to read; the value is
/// already unforgeable without the host key, so the full MAC adds nothing.
const CONFIG_HOME_ID_BYTES: usize = 16;

/// Name of the host-state record holding the revision MAC key.
const REVISION_KEY_RECORD: &str = "profile-revision.key";

/// Bytes of the revision MAC key and of a revision.
const REVISION_BYTES: usize = 32;

type RevisionMac = Hmac<Sha256>;

/// Digest of the launch inputs of one resolved host profile.
///
/// Covers the profile file text (including `[env]` values), the detection
/// manifest text it names, the launch binding of its base runtime and the
/// effective program and arguments (so a different host login shell is a
/// different input). It stays inside the daemon: only its keyed MAC leaves as
/// a [`ProfileRevision`], because an unkeyed digest would let anyone holding
/// it confirm a guessed `[env]` value offline.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProfileInputs([u8; 32]);

impl fmt::Debug for ProfileInputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProfileInputs([REDACTED])")
    }
}

impl ProfileInputs {
    /// Digests the launch inputs. Each part is length-prefixed so adjacent
    /// parts cannot be re-split into the same byte stream.
    pub(crate) fn of(
        profile_text: &str,
        manifest_text: Option<&str>,
        binding_json: &[u8],
        program: &str,
        args: &[String],
    ) -> Result<Self, ProtocolError> {
        let args_json = serde_json::to_vec(args).map_err(|_error| {
            ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_profile",
                "profile launch arguments cannot be encoded",
                None,
            )
        })?;
        let mut hasher = Sha256::new();
        hasher.update(INPUTS_DOMAIN);
        let parts: [Option<&[u8]>; 5] = [
            Some(profile_text.as_bytes()),
            manifest_text.map(str::as_bytes),
            Some(binding_json),
            Some(program.as_bytes()),
            Some(&args_json),
        ];
        for part in parts {
            match part {
                Some(bytes) => {
                    hasher.update([1_u8]);
                    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
                    hasher.update(bytes);
                }
                None => hasher.update([0_u8]),
            }
        }
        Ok(Self(hasher.finalize().into()))
    }
}

/// The host-local secret that keys [`ProfileRevision`]s.
struct RevisionKey(Zeroizing<[u8; REVISION_BYTES]>);

impl fmt::Debug for RevisionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RevisionKey([REDACTED])")
    }
}

impl RevisionKey {
    fn mac(&self, inputs: &ProfileInputs) -> Result<RevisionMac, ProtocolError> {
        let mut mac = RevisionMac::new_from_slice(self.0.as_ref())
            .map_err(|_error| revision_unavailable())?;
        mac.update(REVISION_DOMAIN);
        mac.update(&inputs.0);
        Ok(mac)
    }

    /// Reads the key record of the host-state directory under `state_dir`,
    /// creating it with fresh entropy on first use.
    fn load_or_create(state_dir: &Path) -> Result<Self, ProtocolError> {
        let unavailable = |stage: &'static str, error: &dyn fmt::Display| {
            warn!(stage, error = %error, "profile revision key unavailable");
            revision_unavailable()
        };
        let dir =
            HostStateDir::open_or_create(state_dir).map_err(|error| unavailable("open", &error))?;
        let existing = dir
            .read_secret_record(REVISION_KEY_RECORD)
            .map_err(|error| unavailable("read", &error))?;
        if let Some(bytes) = existing {
            let key: [u8; REVISION_BYTES] = bytes
                .as_slice()
                .try_into()
                .map_err(|_error| unavailable("decode", &"record has the wrong length"))?;
            return Ok(Self(Zeroizing::new(key)));
        }
        let mut key = Zeroizing::new([0_u8; REVISION_BYTES]);
        getrandom::getrandom(key.as_mut()).map_err(|error| unavailable("entropy", &error))?;
        dir.replace_record(REVISION_KEY_RECORD, key.as_ref())
            .map_err(|error| unavailable("write", &error))?;
        Ok(Self(key))
    }
}

fn revision_unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_profile_revision_unavailable",
        "the host cannot read its profile revision key",
        Some("check the owner-private host state directory".to_owned()),
    )
}

/// Lazily loaded [`RevisionKey`] of one host.
///
/// Owner-local launches never need it; only a relay-selected launch or an
/// approval does. A missing state directory or an unreadable key is a typed
/// error, never an unkeyed fallback.
#[derive(Debug, Default)]
pub(crate) struct RevisionKeys {
    state_dir: Option<PathBuf>,
    key: Mutex<Option<Arc<RevisionKey>>>,
}

impl RevisionKeys {
    /// Keys stored under the host-state directory `state_dir`.
    pub(crate) fn new(state_dir: Option<PathBuf>) -> Self {
        Self {
            state_dir,
            key: Mutex::new(None),
        }
    }

    fn key(&self) -> Result<Arc<RevisionKey>, ProtocolError> {
        let state_dir = self.state_dir.as_deref().ok_or_else(revision_unavailable)?;
        let mut cached = self
            .key
            .lock()
            .map_err(|_poisoned| revision_unavailable())?;
        if let Some(key) = cached.as_ref() {
            return Ok(Arc::clone(key));
        }
        let key = Arc::new(RevisionKey::load_or_create(state_dir)?);
        *cached = Some(Arc::clone(&key));
        Ok(key)
    }

    /// The revision of a profile with launch inputs `inputs`.
    ///
    /// Freezes the profile into a session at creation and approves it for a
    /// relay launch.
    pub(crate) fn revision(
        &self,
        inputs: &ProfileInputs,
    ) -> Result<ProfileRevision, ProtocolError> {
        let tag = self.key()?.mac(inputs)?.finalize().into_bytes();
        Ok(ProfileRevision(tag.into()))
    }

    /// The opaque identifier of the config home at `home`: a keyed MAC of the
    /// directory, truncated and hex encoded.
    ///
    /// The directory is guessable, so an unkeyed digest would let anyone holding
    /// the identifier confirm a path offline; under the host key it reveals
    /// nothing about the path and cannot be compared across hosts.
    ///
    /// # Errors
    ///
    /// Returns `agent_profile_revision_unavailable` when the host's key cannot
    /// be read or created.
    pub(crate) fn config_home_id(&self, home: &Path) -> Result<String, ProtocolError> {
        let key = self.key()?;
        let mut mac =
            RevisionMac::new_from_slice(key.0.as_ref()).map_err(|_error| revision_unavailable())?;
        mac.update(CONFIG_HOME_ID_DOMAIN);
        mac.update(home.as_os_str().as_encoded_bytes());
        let tag = mac.finalize().into_bytes();
        Ok(lower_hex(&tag[..CONFIG_HOME_ID_BYTES]))
    }

    /// Whether `revision` is the current revision of a profile with `inputs`,
    /// compared in constant time.
    pub(crate) fn matches(
        &self,
        inputs: &ProfileInputs,
        revision: &ProfileRevision,
    ) -> Result<bool, ProtocolError> {
        Ok(self.key()?.mac(inputs)?.verify_slice(&revision.0).is_ok())
    }
}

/// The revision of one resolved host profile: a keyed MAC over its
/// [`ProfileInputs`], bound to this host's revision key.
///
/// Any edit to the inputs yields a different revision. Without the host key a
/// revision reveals nothing about `[env]` values, so it may be stored and
/// serialized as its 64-digit lowercase hex form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRevision([u8; REVISION_BYTES]);

impl serde::Serialize for ProfileRevision {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for ProfileRevision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(|error| serde::de::Error::custom(error.msg))
    }
}

impl ProfileRevision {
    /// Parses the textual form of a revision.
    ///
    /// # Errors
    ///
    /// Returns `agent_profile_revision_invalid` unless `value` is exactly 64
    /// lowercase hex digits.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        let invalid = || {
            ProtocolError::new(
                ErrorClass::Runtime,
                "agent_profile_revision_invalid",
                "a profile revision is 64 lowercase hex digits",
                None,
            )
        };
        let digits = value.as_bytes();
        if digits.len() != REVISION_BYTES * 2 {
            return Err(invalid());
        }
        let mut bytes = [0_u8; REVISION_BYTES];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let high = hex_digit(digits[index * 2]).ok_or_else(invalid)?;
            let low = hex_digit(digits[index * 2 + 1]).ok_or_else(invalid)?;
            *slot = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

/// The lowercase hex text of `bytes`.
fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for ProfileRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// How a launch request identifies the agent it wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSource {
    /// A name chosen by the owner on this host: a host profile, else an
    /// installed runtime id.
    OwnerLocal {
        /// The agent name as typed by the owner.
        name: String,
    },
    /// A profile the owner approved for relay launches, at the revision that
    /// was approved. Never a runtime id, package, program or argv.
    RelayProfile {
        /// The approved host profile name.
        profile: String,
        /// The revision the owner approved.
        revision: ProfileRevision,
    },
}

impl LaunchSource {
    /// Resolves the source to the agent to launch.
    ///
    /// A relay source checks the approved revision against the profile's
    /// current launch inputs with the host's revision key.
    ///
    /// # Errors
    ///
    /// Owner-local resolution returns the profile registry's errors. A relay
    /// source returns `agent_profile_not_found` when the name is not a host
    /// profile (a bare runtime id included) and `agent_profile_revision_stale`
    /// when the profile no longer has the approved revision.
    pub(crate) fn resolve(
        &self,
        profiles: &ProfileRegistry,
    ) -> Result<ResolvedAgent, ProtocolError> {
        match self {
            Self::OwnerLocal { name } => profiles.resolve_agent(name),
            Self::RelayProfile { profile, revision } => {
                let agent = profiles.resolve_agent(profile)?;
                let Some(inputs) = agent.profile_inputs() else {
                    return Err(agent_profile_not_found(profile));
                };
                if profiles.revision_keys().matches(inputs, revision)? {
                    Ok(agent)
                } else {
                    Err(ProtocolError::new(
                        ErrorClass::Runtime,
                        "agent_profile_revision_stale",
                        format!("agent profile '{profile}' changed after it was approved"),
                        Some("approve the current profile revision again".to_owned()),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::host::RuntimeHost;

    fn scoped(tag: &str) -> crate::test_support::ScopedDir {
        crate::test_support::scoped_dir(&format!("pohunek-source-{tag}-"))
    }

    fn write_file(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write file");
    }

    fn write_profile(dir: &Path, name: &str, body: &str) {
        write_file(&dir.join(format!("{name}.toml")), body);
    }

    /// A registry over `agents` whose revisions are keyed under `state`.
    fn registry(agents: &Path, state: &Path) -> ProfileRegistry {
        ProfileRegistry::new(Some(agents.to_path_buf()))
            .with_revision_state_dir(Some(state.to_path_buf()))
    }

    fn revision_of(profiles: &ProfileRegistry, name: &str) -> ProfileRevision {
        let agent = profiles.resolve_agent(name).expect("profile resolves");
        profiles
            .revision_of(&agent)
            .expect("key available")
            .expect("a profile carries a revision")
    }

    fn relay(profile: &str, revision: &ProfileRevision) -> LaunchSource {
        LaunchSource::RelayProfile {
            profile: profile.to_owned(),
            revision: revision.clone(),
        }
    }

    fn stale_code(profiles: &ProfileRegistry, name: &str, approved: &ProfileRevision) -> String {
        relay(name, approved)
            .resolve(profiles)
            .expect_err("must be rejected")
            .code
            .clone()
    }

    #[test]
    fn a_revision_round_trips_through_json_as_its_hex_form() {
        let (agents, state) = (scoped("revision-json"), scoped("revision-json-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = registry(&agents, &state);
        let revision = revision_of(&profiles, "wrapped");

        let json = serde_json::to_value(&revision).expect("serialize");
        assert_eq!(json, serde_json::Value::String(revision.to_string()));
        let back: ProfileRevision = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, revision);
        for malformed in [
            "",
            "abc",
            &"G".repeat(64),
            &revision.to_string().to_uppercase(),
        ] {
            assert!(
                serde_json::from_value::<ProfileRevision>(serde_json::json!(malformed)).is_err(),
                "{malformed}"
            );
        }
    }

    #[test]
    fn owner_local_resolves_profiles_and_bare_runtimes() {
        let agents = scoped("owner");
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = ProfileRegistry::new(Some(agents.to_path_buf()));

        let wrapped = LaunchSource::OwnerLocal {
            name: "wrapped".to_owned(),
        }
        .resolve(&profiles)
        .expect("profile resolves");
        assert!(wrapped.profile_inputs().is_some());

        let bare = LaunchSource::OwnerLocal {
            name: "shell".to_owned(),
        }
        .resolve(&profiles)
        .expect("bare runtime resolves");
        assert!(bare.profile_inputs().is_none());

        let unknown = LaunchSource::OwnerLocal {
            name: "nope".to_owned(),
        }
        .resolve(&profiles)
        .expect_err("unknown name");
        assert_eq!(unknown.code, "agent_profile_not_found");
    }

    #[test]
    fn relay_resolves_an_approved_profile() {
        let (agents, state) = (scoped("relay-ok"), scoped("relay-ok-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = registry(&agents, &state);
        let approved = revision_of(&profiles, "wrapped");

        let agent = relay("wrapped", &approved)
            .resolve(&profiles)
            .expect("approved revision resolves");
        assert_eq!(agent.name, "wrapped");
    }

    #[test]
    fn relay_rejects_a_profile_edited_after_approval() {
        let (agents, state) = (scoped("relay-stale"), scoped("relay-stale-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = registry(&agents, &state);
        let approved = revision_of(&profiles, "wrapped");

        write_profile(&agents, "wrapped", "base = \"shell\"\nargs = [\"-l\"]\n");
        let error = relay("wrapped", &approved)
            .resolve(&profiles)
            .expect_err("edited profile is stale");
        assert_eq!(error.code, "agent_profile_revision_stale");
        assert!(!error.msg.contains(&approved.to_string()));
    }

    #[test]
    fn relay_rejects_an_edited_detection_manifest() {
        let (agents, state) = (scoped("relay-manifest"), scoped("relay-manifest-state"));
        let manifests = agents.join("manifests");
        std::fs::create_dir_all(&manifests).expect("manifests dir");
        let manifest = |text: &str| {
            format!(
                "[[rules]]\nid = \"custom\"\nstate = \"blocked\"\npriority = 1\nregion = \"whole_recent\"\ncontains = \"{text}\"\n"
            )
        };
        write_file(&manifests.join("m.toml"), &manifest("one"));
        write_profile(&agents, "wrapped", "base = \"shell\"\nmanifest = \"m\"\n");
        let profiles = registry(&agents, &state);
        let approved = revision_of(&profiles, "wrapped");
        relay("wrapped", &approved)
            .resolve(&profiles)
            .expect("approved manifest resolves");

        write_file(&manifests.join("m.toml"), &manifest("two"));
        assert_eq!(
            stale_code(&profiles, "wrapped", &approved),
            "agent_profile_revision_stale"
        );
    }

    #[test]
    fn revisions_of_profiles_differing_only_in_a_guessable_env_value_are_unrelated() {
        let state = scoped("env-state");
        let revision_for = |tag: &str, value: &str| {
            let agents = scoped(tag);
            write_profile(
                &agents,
                "wrapped",
                &format!("base = \"shell\"\n[env]\nMODE = \"{value}\"\n"),
            );
            revision_of(&registry(&agents, &state), "wrapped")
        };
        let one = revision_for("env-a", "1234");
        let two = revision_for("env-b", "1235");
        assert_ne!(one, two);
        assert_eq!(one, revision_for("env-c", "1234"));

        // The revision is not the unkeyed digest of the inputs: without the
        // host key, a guessed value cannot be checked against it.
        let agents = scoped("env-d");
        write_profile(
            &agents,
            "wrapped",
            "base = \"shell\"\n[env]\nMODE = \"1234\"\n",
        );
        let profiles = registry(&agents, &state);
        let agent = profiles.resolve_agent("wrapped").expect("resolves");
        let inputs = agent.profile_inputs().expect("inputs");
        assert_ne!(one.to_string(), hex_of(&inputs.0));
    }

    fn hex_of(bytes: &[u8; REVISION_BYTES]) -> String {
        ProfileRevision(*bytes).to_string()
    }

    #[test]
    fn a_different_host_key_yields_an_unrelated_revision() {
        let agents = scoped("keys-agents");
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let (state_a, state_b) = (scoped("keys-a"), scoped("keys-b"));
        assert_ne!(
            revision_of(&registry(&agents, &state_a), "wrapped"),
            revision_of(&registry(&agents, &state_b), "wrapped")
        );
    }

    #[test]
    fn revisions_are_stable_across_registry_restarts() {
        let (agents, state) = (scoped("restart"), scoped("restart-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let first = revision_of(&registry(&agents, &state), "wrapped");
        let restarted = registry(&agents, &state);
        assert_eq!(revision_of(&restarted, "wrapped"), first);
        relay("wrapped", &first)
            .resolve(&restarted)
            .expect("an approved revision survives a restart");
    }

    #[test]
    fn the_key_record_is_owner_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let (agents, state) = (scoped("mode"), scoped("mode-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        revision_of(&registry(&agents, &state), "wrapped");
        let record = state.join("host").join(REVISION_KEY_RECORD);
        let mode = std::fs::metadata(&record)
            .expect("key record")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "key record must not be group/world accessible"
        );
    }

    #[test]
    fn a_missing_or_unusable_key_is_a_typed_error_never_a_fallback() {
        let agents = scoped("nokey");
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let keyed = registry(&agents, &scoped("nokey-state"));
        let approved = revision_of(&keyed, "wrapped");

        let unkeyed = ProfileRegistry::new(Some(agents.to_path_buf()));
        let agent = unkeyed.resolve_agent("wrapped").expect("resolves");
        assert_eq!(
            unkeyed.revision_of(&agent).expect_err("no key").code,
            "agent_profile_revision_unavailable"
        );
        assert_eq!(
            stale_code(&unkeyed, "wrapped", &approved),
            "agent_profile_revision_unavailable"
        );

        let state = scoped("badkey-state");
        let broken = registry(&agents, &state);
        revision_of(&broken, "wrapped");
        let record = state.join("host").join(REVISION_KEY_RECORD);
        std::fs::write(&record, b"short").expect("corrupt key");
        let fresh = registry(&agents, &state);
        let agent = fresh.resolve_agent("wrapped").expect("resolves");
        assert_eq!(
            fresh.revision_of(&agent).expect_err("corrupt key").code,
            "agent_profile_revision_unavailable"
        );
    }

    #[test]
    fn identical_shell_profiles_under_different_login_shells_have_different_revisions() {
        let (agents, state) = (scoped("login"), scoped("login-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let under = |shell: &str| {
            let host = RuntimeHost::new(
                crate::agent::host::RuntimeRegistry::from_sources(&[
                    &crate::agent::host::BuiltinSource::from_login_shell(Some(shell.to_owned())),
                ])
                .expect("registry"),
            );
            let profiles = ProfileRegistry::with_runtimes(Some(agents.to_path_buf()), host)
                .with_revision_state_dir(Some(state.to_path_buf()));
            revision_of(&profiles, "wrapped")
        };
        let zsh = under("/usr/bin/zsh");
        assert_eq!(zsh, under("/usr/bin/zsh"));
        assert_ne!(zsh, under("/usr/bin/fish"));
    }

    #[test]
    fn relay_can_never_name_a_runtime_id() {
        let (agents, state) = (scoped("relay-bare"), scoped("relay-bare-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = registry(&agents, &state);
        let some_revision = revision_of(&profiles, "wrapped");

        for runtime in ["shell", "codex", "claude", "hermes"] {
            assert_eq!(
                stale_code(&profiles, runtime, &some_revision),
                "agent_profile_not_found",
                "{runtime}"
            );
        }
    }

    #[test]
    fn relay_cannot_smuggle_a_path_or_program_as_a_profile_name() {
        let (agents, state) = (scoped("relay-names"), scoped("relay-names-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = registry(&agents, &state);
        let revision = revision_of(&profiles, "wrapped");
        for name in ["../shell", "/bin/sh", "a/b", "sh -c x", ""] {
            let error = relay(name, &revision)
                .resolve(&profiles)
                .expect_err("not a profile name");
            assert_eq!(error.class, ErrorClass::Runtime, "{name}");
        }
    }

    #[test]
    fn relay_profile_that_does_not_exist_is_not_found() {
        let (agents, state) = (scoped("relay-missing"), scoped("relay-missing-state"));
        write_profile(&agents, "wrapped", "base = \"shell\"\n");
        let profiles = registry(&agents, &state);
        let revision = revision_of(&profiles, "wrapped");
        assert_eq!(
            stale_code(&profiles, "ghost", &revision),
            "agent_profile_not_found"
        );
    }

    #[test]
    fn inputs_separate_their_parts() {
        let digest = |p: &str, m: Option<&str>, b: &[u8], program: &str, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            ProfileInputs::of(p, m, b, program, &args).expect("inputs")
        };
        let base = digest("p", Some("m"), b"b", "sh", &["-l"]);
        assert_eq!(base, digest("p", Some("m"), b"b", "sh", &["-l"]));
        assert_ne!(base, digest("pm", None, b"b", "sh", &["-l"]));
        assert_ne!(base, digest("p", None, b"b", "sh", &["-l"]));
        assert_ne!(
            digest("p", Some(""), b"b", "sh", &[]),
            digest("p", None, b"b", "sh", &[])
        );
        assert_ne!(base, digest("p", Some("m"), b"b", "zsh", &["-l"]));
        assert_ne!(base, digest("p", Some("m"), b"b", "sh", &["-i"]));
        assert_ne!(
            digest("p", None, b"b", "sh", &["a", "b"]),
            digest("p", None, b"b", "sh", &["ab"])
        );
    }

    #[test]
    fn revision_text_roundtrips_and_rejects_malformed_text() {
        let revision = ProfileRevision([0xab; REVISION_BYTES]);
        let text = revision.to_string();
        assert_eq!(text.len(), REVISION_BYTES * 2);
        assert_eq!(ProfileRevision::parse(&text).expect("roundtrip"), revision);
        let upper = text.to_ascii_uppercase();
        for bad in [
            "",
            "abc",
            upper.as_str(),
            &text[1..],
            &format!("{text}0"),
            "zz",
        ] {
            let error = ProfileRevision::parse(bad).expect_err("malformed revision");
            assert_eq!(error.code, "agent_profile_revision_invalid", "{bad}");
        }
    }

    /// A host-state directory the key record can live in.
    fn private_state(tag: &str) -> crate::test_support::ScopedDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scoped(tag);
        std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o700))
            .expect("make the state directory owner-private");
        dir
    }

    #[test]
    fn config_home_ids_are_keyed_stable_and_path_sensitive() {
        let state = private_state("home-id");
        let keys = RevisionKeys::new(Some(state.to_path_buf()));
        let home = Path::new("/srv/accounts/zq-distinct-home");

        let id = keys.config_home_id(home).expect("key available");

        assert_eq!(id.len(), CONFIG_HOME_ID_BYTES * 2);
        assert!(id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_eq!(id, keys.config_home_id(home).expect("second call"));
        // A cold cache reads the same stored key, as after a restart.
        let restarted = RevisionKeys::new(Some(state.to_path_buf()));
        assert_eq!(id, restarted.config_home_id(home).expect("restarted"));
        assert_ne!(
            id,
            keys.config_home_id(Path::new("/srv/accounts/zq-other-home"))
                .expect("another home")
        );
        // An unkeyed digest of the path would let anyone confirm a guess.
        let unkeyed = Sha256::digest(home.as_os_str().as_encoded_bytes());
        assert_ne!(id, lower_hex(&unkeyed[..CONFIG_HOME_ID_BYTES]));
        // Another host's key identifies the same directory differently.
        let other = RevisionKeys::new(Some(private_state("home-id-other").to_path_buf()));
        assert_ne!(id, other.config_home_id(home).expect("another host"));
    }

    #[test]
    fn config_home_ids_without_a_key_are_a_typed_error() {
        let error = RevisionKeys::new(None)
            .config_home_id(Path::new("/srv/accounts/home"))
            .expect_err("no state directory, no key");
        assert_eq!(error.code, "agent_profile_revision_unavailable");
    }
}
