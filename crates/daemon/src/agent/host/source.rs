//! Where a launch request gets its agent from: the owner's host or a relay.
//!
//! A [`LaunchSource`] is the only way a launch names the agent to run. The
//! owner-local source resolves a name on this host (a host profile, else an
//! installed runtime id). The relay source resolves ONLY a profile the owner
//! has locally approved, identified by its name and the [`ProfileRevision`]
//! that was approved: it can never name a runtime id, package, program or
//! argv, and an edited profile fails with `agent_profile_revision_stale`
//! instead of launching something the owner did not approve.
//!
//! Wiring a relay-origin `session.new` to [`LaunchSource::RelayProfile`]
//! depends on the locally approved `HostShare` of issue #82; until then the
//! relay variant is exercised at this seam only.

// Rust guideline compliant 2026-10-04

use std::fmt;

use protocol::{ErrorClass, ProtocolError};
use sha2::{Digest, Sha256};

use crate::agent::profile::agent_profile_not_found;
use crate::agent::{ProfileRegistry, ResolvedAgent};

/// Domain separator of the revision digest; a different input shape would
/// need a different tag so digests never collide across shapes.
const REVISION_DOMAIN: &[u8] = b"pohunek.agent-profile-revision";

/// Length of a revision in lowercase hex digits (a SHA-256 digest).
const REVISION_HEX_LEN: usize = 64;

/// The revision of one resolved host profile.
///
/// A digest over the profile file text, the detection manifest text it names
/// (when any) and the launch binding of its base runtime. Any edit to those
/// inputs, including a comment or an `[env]` value, yields a different
/// revision; the digest is one-way, so it reveals no environment value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRevision(String);

impl ProfileRevision {
    /// Digest of a profile's launch inputs.
    ///
    /// Each part is length-prefixed so adjacent parts cannot be re-split into
    /// the same byte stream.
    #[must_use]
    pub(crate) fn of_inputs(
        profile_text: &str,
        manifest_text: Option<&str>,
        binding_json: &[u8],
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(REVISION_DOMAIN);
        let parts: [Option<&[u8]>; 3] = [
            Some(profile_text.as_bytes()),
            manifest_text.map(str::as_bytes),
            Some(binding_json),
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
        Self(hex_lower(&hasher.finalize()))
    }

    /// Parses the textual form of a revision.
    ///
    /// # Errors
    ///
    /// Returns `agent_profile_revision_invalid` unless `value` is exactly 64
    /// lowercase hex digits.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        if value.len() == REVISION_HEX_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_profile_revision_invalid",
                "a profile revision is 64 lowercase hex digits",
                None,
            ))
        }
    }

    /// The textual form of the revision.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    use fmt::Write as _;

    bytes.iter().fold(String::new(), |mut out, byte| {
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
        out
    })
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
    /// The resolved agent keeps the revision of the profile it came from
    /// (`ResolvedAgent::profile_revision`).
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
                match agent.profile_revision() {
                    None => Err(agent_profile_not_found(profile)),
                    Some(current) if current == revision => Ok(agent),
                    Some(_) => Err(ProtocolError::new(
                        ErrorClass::Runtime,
                        "agent_profile_revision_stale",
                        format!("agent profile '{profile}' changed after it was approved"),
                        Some("approve the current profile revision again".to_owned()),
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents_dir(tag: &str) -> crate::test_support::ScopedDir {
        crate::test_support::scoped_dir(&format!("pohunek-source-{tag}-"))
    }

    fn write_profile(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(format!("{name}.toml")), body).expect("write profile");
    }

    fn revision_of(profiles: &ProfileRegistry, name: &str) -> ProfileRevision {
        profiles
            .resolve_agent(name)
            .expect("profile resolves")
            .profile_revision()
            .expect("a profile carries a revision")
            .clone()
    }

    fn relay(profile: &str, revision: &ProfileRevision) -> LaunchSource {
        LaunchSource::RelayProfile {
            profile: profile.to_owned(),
            revision: revision.clone(),
        }
    }

    #[test]
    fn owner_local_resolves_profiles_and_bare_runtimes() {
        let dir = agents_dir("owner");
        write_profile(&dir, "wrapped", "base = \"shell\"\n");
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));

        let wrapped = LaunchSource::OwnerLocal {
            name: "wrapped".to_owned(),
        }
        .resolve(&profiles)
        .expect("profile resolves");
        assert!(wrapped.profile_revision().is_some());

        let bare = LaunchSource::OwnerLocal {
            name: "shell".to_owned(),
        }
        .resolve(&profiles)
        .expect("bare runtime resolves");
        assert!(bare.profile_revision().is_none());

        let unknown = LaunchSource::OwnerLocal {
            name: "nope".to_owned(),
        }
        .resolve(&profiles)
        .expect_err("unknown name");
        assert_eq!(unknown.code, "agent_profile_not_found");
    }

    #[test]
    fn relay_resolves_an_approved_profile_and_keeps_its_revision() {
        let dir = agents_dir("relay-ok");
        write_profile(&dir, "wrapped", "base = \"shell\"\n");
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));
        let approved = revision_of(&profiles, "wrapped");

        let agent = relay("wrapped", &approved)
            .resolve(&profiles)
            .expect("approved revision resolves");
        assert_eq!(agent.name, "wrapped");
        assert_eq!(agent.profile_revision(), Some(&approved));
    }

    #[test]
    fn relay_rejects_a_profile_edited_after_approval() {
        let dir = agents_dir("relay-stale");
        write_profile(&dir, "wrapped", "base = \"shell\"\n");
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));
        let approved = revision_of(&profiles, "wrapped");

        write_profile(&dir, "wrapped", "base = \"shell\"\nargs = [\"-l\"]\n");
        let error = relay("wrapped", &approved)
            .resolve(&profiles)
            .expect_err("edited profile is stale");
        assert_eq!(error.code, "agent_profile_revision_stale");
        assert!(!error.msg.contains(approved.as_str()));
    }

    #[test]
    fn relay_rejects_a_changed_env_value_and_manifest() {
        let dir = agents_dir("relay-env");
        write_profile(
            &dir,
            "wrapped",
            "base = \"shell\"\n[env]\nTOKEN_NAME = \"one\"\n",
        );
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));
        let approved = revision_of(&profiles, "wrapped");

        write_profile(
            &dir,
            "wrapped",
            "base = \"shell\"\n[env]\nTOKEN_NAME = \"two\"\n",
        );
        let changed = revision_of(&profiles, "wrapped");
        assert_ne!(changed, approved);
        assert_eq!(
            relay("wrapped", &approved)
                .resolve(&profiles)
                .expect_err("changed env value is stale")
                .code,
            "agent_profile_revision_stale"
        );
    }

    #[test]
    fn relay_can_never_name_a_runtime_id() {
        let dir = agents_dir("relay-bare");
        write_profile(&dir, "wrapped", "base = \"shell\"\n");
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));
        let some_revision = revision_of(&profiles, "wrapped");

        for runtime in ["shell", "codex", "claude", "hermes"] {
            let error = relay(runtime, &some_revision)
                .resolve(&profiles)
                .expect_err("a bare runtime id is not a profile");
            assert_eq!(error.code, "agent_profile_not_found", "{runtime}");
        }
    }

    #[test]
    fn relay_cannot_smuggle_a_path_or_program_as_a_profile_name() {
        let dir = agents_dir("relay-names");
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));
        let revision = ProfileRevision::of_inputs("", None, b"");
        for name in ["../shell", "/bin/sh", "a/b", "sh -c x", ""] {
            let error = relay(name, &revision)
                .resolve(&profiles)
                .expect_err("not a profile name");
            assert_eq!(error.class, ErrorClass::Runtime, "{name}");
        }
    }

    #[test]
    fn relay_profile_that_does_not_exist_is_not_found() {
        let dir = agents_dir("relay-missing");
        let profiles = ProfileRegistry::new(Some(dir.to_path_buf()));
        let revision = ProfileRevision::of_inputs("", None, b"");
        let error = relay("ghost", &revision)
            .resolve(&profiles)
            .expect_err("no such profile");
        assert_eq!(error.code, "agent_profile_not_found");
    }

    #[test]
    fn revision_is_stable_and_separates_its_inputs() {
        let a = ProfileRevision::of_inputs("p", Some("m"), b"b");
        assert_eq!(a, ProfileRevision::of_inputs("p", Some("m"), b"b"));
        assert_ne!(a, ProfileRevision::of_inputs("pm", None, b"b"));
        assert_ne!(a, ProfileRevision::of_inputs("p", None, b"b"));
        assert_ne!(
            ProfileRevision::of_inputs("p", Some(""), b"b"),
            ProfileRevision::of_inputs("p", None, b"b")
        );
        assert_ne!(
            ProfileRevision::of_inputs("ab", Some("c"), b""),
            ProfileRevision::of_inputs("a", Some("bc"), b"")
        );
        assert_eq!(ProfileRevision::parse(a.as_str()).expect("roundtrip"), a);
    }

    #[test]
    fn revision_parse_rejects_malformed_text() {
        let valid = ProfileRevision::of_inputs("p", None, b"b");
        let upper = valid.as_str().to_ascii_uppercase();
        for bad in ["", "abc", upper.as_str(), &valid.as_str()[1..], "zz"] {
            let error = ProfileRevision::parse(bad).expect_err("malformed revision");
            assert_eq!(error.code, "agent_profile_revision_invalid", "{bad}");
        }
    }
}
