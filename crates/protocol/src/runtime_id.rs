//! Agent runtime identity and launch binding wire types.
//!
//! A [`RuntimeId`] names one installed agent runtime (for example `codex`). It
//! is always valid by construction and by deserialization. A [`RuntimeRef`] is
//! the lenient counterpart for values read back from history or from a newer
//! peer: it keeps any string for display, and a grammar-valid one is an
//! [`RuntimeRef::Id`] that a registry may still reject as not installed. A [`LaunchBinding`] pins the runtime identity a session launched
//! with, including an explicit statement of where that identity came from.
//!
//! # Examples
//!
//! ```
//! use protocol::{RuntimeId, RuntimeRef};
//!
//! let id = RuntimeId::parse("codex")?;
//! assert_eq!(id.as_str(), "codex");
//!
//! let historical: RuntimeRef = serde_json::from_str(r#""Not A Valid Id""#)?;
//! assert!(historical.launchable().is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::{ErrorClass, ProtocolError};

/// Maximum UTF-8 bytes in an agent runtime identifier.
///
/// Runtime identifiers appear in session records, hooks, profiles and
/// notifications; 64 bytes fits every single-segment name used today with
/// ample headroom while keeping each carrying record bounded. Raising it widens
/// every record that embeds an identifier.
pub const MAX_AGENT_RUNTIME_ID_BYTES: usize = 64;

/// Maximum UTF-8 bytes in a runtime package identifier.
///
/// Package identifiers are dotted names such as `pohunek.runtime.codex`; the
/// larger ceiling leaves room for namespaced third-party packages.
pub const MAX_PACKAGE_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes in a runtime package version string.
pub const MAX_PACKAGE_VERSION_BYTES: usize = 64;

/// Prefix that marks a content digest on the wire.
const DIGEST_PREFIX: &str = "sha256:";

/// Number of lowercase hexadecimal characters in a SHA-256 digest.
const DIGEST_HEX_CHARS: usize = 64;

/// Why a runtime or package identifier is invalid.
///
/// Variants never carry the rejected text, so a diagnostic cannot echo
/// attacker-chosen content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RuntimeIdError {
    /// The identifier is empty.
    #[error("{type_name} cannot be empty")]
    Empty {
        /// Name of the rejected type.
        type_name: &'static str,
    },
    /// The identifier exceeds the byte limit.
    #[error("{type_name} cannot exceed {max} bytes")]
    TooLong {
        /// Name of the rejected type.
        type_name: &'static str,
        /// Byte limit that was exceeded.
        max: usize,
    },
    /// A byte is outside lowercase ASCII alphanumerics, `.`, `_` and `-`.
    #[error("{type_name} contains a character outside [a-z0-9._-] at byte {index}")]
    InvalidCharacter {
        /// Name of the rejected type.
        type_name: &'static str,
        /// Zero-based byte index of the first offending byte.
        index: usize,
    },
    /// The identifier starts with `.` or `-`.
    #[error("{type_name} cannot start with `.` or `-`")]
    LeadingSeparator {
        /// Name of the rejected type.
        type_name: &'static str,
    },
    /// The identifier contains two consecutive dots.
    #[error("{type_name} cannot contain `..`")]
    ConsecutiveDots {
        /// Name of the rejected type.
        type_name: &'static str,
    },
}

/// Why a package version or content digest is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BindingFieldError {
    /// The package version is empty, too long, or has a character outside
    /// ASCII alphanumerics and `.`, `+`, `-`, `_`.
    #[error("package version must be 1..={MAX_PACKAGE_VERSION_BYTES} bytes of [A-Za-z0-9.+_-]")]
    Version,
    /// The digest is not `sha256:` followed by 64 lowercase hex characters.
    #[error("digest must be `sha256:` followed by 64 lowercase hex characters")]
    Digest,
}

fn validate_identifier(
    value: &str,
    type_name: &'static str,
    max: usize,
) -> Result<(), RuntimeIdError> {
    if value.is_empty() {
        return Err(RuntimeIdError::Empty { type_name });
    }
    if value.len() > max {
        return Err(RuntimeIdError::TooLong { type_name, max });
    }
    if let Some(index) = value.bytes().position(|byte| {
        !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte))
    }) {
        return Err(RuntimeIdError::InvalidCharacter { type_name, index });
    }
    if value.starts_with(['.', '-']) {
        return Err(RuntimeIdError::LeadingSeparator { type_name });
    }
    if value.contains("..") {
        return Err(RuntimeIdError::ConsecutiveDots { type_name });
    }
    Ok(())
}

/// Declares one validated string newtype with strict (de)serialization.
macro_rules! validated_string {
    (
        $(#[$docs:meta])*
        $name:ident, $export:literal, $error:ty, $validate:expr
    ) => {
        $(#[$docs])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        #[cfg_attr(feature = "ts", ts(export, export_to = $export, type = "string"))]
        pub struct $name(String);

        impl $name {
            /// Parses and validates one wire value.
            ///
            /// # Errors
            ///
            /// Returns the first violated rule.
            pub fn parse(value: &str) -> Result<Self, $error> {
                let validate: fn(&str) -> Result<(), $error> = $validate;
                validate(value)?;
                Ok(Self(value.to_owned()))
            }

            /// Returns the validated wire value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = $error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = $error;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

validated_string!(
    /// Identity of one installed agent runtime, valid by construction.
    ///
    /// Lowercase ASCII alphanumerics plus `.`, `_` and `-`; non-empty, at most
    /// [`MAX_AGENT_RUNTIME_ID_BYTES`] bytes, no leading `.` or `-`, no `..`.
    /// Deserialization applies the same rules and rejects anything else.
    RuntimeId,
    "RuntimeId.ts",
    RuntimeIdError,
    |value| validate_identifier(value, "RuntimeId", MAX_AGENT_RUNTIME_ID_BYTES)
);

validated_string!(
    /// Identity of one runtime package, valid by construction.
    ///
    /// Same character rules as [`RuntimeId`] with a
    /// [`MAX_PACKAGE_ID_BYTES`] ceiling. A package id is not a runtime selector.
    PackageId,
    "PackageId.ts",
    RuntimeIdError,
    |value| validate_identifier(value, "PackageId", MAX_PACKAGE_ID_BYTES)
);

validated_string!(
    /// Version string of one runtime package.
    ///
    /// Opaque to the protocol: 1..=[`MAX_PACKAGE_VERSION_BYTES`] bytes of ASCII
    /// alphanumerics and `.`, `+`, `_`, `-`. Version ordering belongs to the
    /// registry, not to the wire.
    PackageVersion,
    "PackageVersion.ts",
    BindingFieldError,
    validate_version
);

validated_string!(
    /// Digest of a runtime package archive.
    ///
    /// `sha256:` followed by 64 lowercase hex characters. It authenticates a
    /// package archive and is never derived for a built-in runtime; see
    /// [`DescriptorDigest`] for the built-in counterpart.
    PackageDigest,
    "PackageDigest.ts",
    BindingFieldError,
    validate_digest
);

validated_string!(
    /// Digest of a built-in runtime descriptor's structural launch fields.
    ///
    /// Same `sha256:` syntax as [`PackageDigest`], but it covers only the
    /// fields that change how a session launches, resumes, forks and frames
    /// input. It is not a package digest and says nothing about archive
    /// contents; the two types are deliberately not interchangeable.
    DescriptorDigest,
    "DescriptorDigest.ts",
    BindingFieldError,
    validate_digest
);

fn validate_version(value: &str) -> Result<(), BindingFieldError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_PACKAGE_VERSION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+_-".contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(BindingFieldError::Version)
    }
}

fn validate_digest(value: &str) -> Result<(), BindingFieldError> {
    let valid = value.strip_prefix(DIGEST_PREFIX).is_some_and(|hex| {
        hex.len() == DIGEST_HEX_CHARS
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(BindingFieldError::Digest)
    }
}

impl RuntimeId {
    /// Wire value of the shell runtime, the only runtime without a package.
    pub const SHELL: &'static str = "shell";

    /// Wraps a literal the crate itself guarantees valid (covered by tests).
    pub(crate) fn from_trusted(value: &'static str) -> Self {
        Self(value.to_owned())
    }
}

/// A runtime reference read from the wire or from history.
///
/// Deserialization accepts any string: a grammar-valid [`RuntimeId`] becomes
/// [`RuntimeRef::Id`], anything else is kept verbatim as
/// [`RuntimeRef::Historical`]. The wire form is a bare string, so a value
/// round-trips to the same variant. `Id` does not mean launchable: whether a
/// runtime may be launched is decided by the registry at resolve time, which
/// answers a valid but uninstalled id with `runtime_not_installed`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "RuntimeRef.ts", type = "string")
)]
pub enum RuntimeRef {
    /// A grammar-valid runtime identity, which the registry may or may not
    /// have installed.
    Id(RuntimeId),
    /// A label that is not a valid [`RuntimeId`]: displayable, never
    /// launchable. [`HistoricalRuntime`] cannot hold a grammar-valid string,
    /// so the variant always survives a wire roundtrip.
    Historical(HistoricalRuntime),
}

/// A runtime label that is not a grammar-valid [`RuntimeId`].
///
/// The private field makes the invariant unbreakable: the only constructors
/// reject any string that parses as a [`RuntimeId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HistoricalRuntime(String);

impl HistoricalRuntime {
    /// Wraps `value`, or returns `None` when it is a valid [`RuntimeId`] and
    /// therefore belongs in [`RuntimeRef::Id`].
    #[must_use]
    pub fn new(value: &str) -> Option<Self> {
        RuntimeId::parse(value)
            .is_err()
            .then(|| Self(value.to_owned()))
    }

    /// The label text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for HistoricalRuntime {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl RuntimeRef {
    /// Classifies a wire string.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        RuntimeId::parse(value).map_or_else(
            |_error| Self::Historical(HistoricalRuntime(value.to_owned())),
            Self::Id,
        )
    }

    /// Returns the wire value.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Id(id) => id.as_str(),
            Self::Historical(label) => label.as_str(),
        }
    }

    /// Returns the identity to hand to a registry for resolution.
    ///
    /// This only rejects values that are not grammar-valid runtime ids; it
    /// does not check installation. A valid id that no enabled runtime
    /// resolves is rejected later with `runtime_not_installed`.
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` with a fixed message for a historical
    /// label. The message never includes the label, which is unvalidated text.
    pub fn launchable(&self) -> Result<&RuntimeId, ProtocolError> {
        match self {
            Self::Id(id) => Ok(id),
            Self::Historical(_) => Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_kind_unsupported",
                "the agent kind is presentation-only and cannot be mutated or persisted",
                Some(
                    "upgrade the daemon to a version that explicitly supports this agent kind"
                        .to_owned(),
                ),
            )),
        }
    }
}

impl Display for RuntimeRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl From<RuntimeId> for RuntimeRef {
    fn from(id: RuntimeId) -> Self {
        Self::Id(id)
    }
}

impl Serialize for RuntimeRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for RuntimeRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&value))
    }
}

/// Identity of one runtime package: its id and exact version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageIdentity.ts"))]
#[serde(deny_unknown_fields)]
pub struct PackageIdentity {
    /// Package id.
    pub id: PackageId,
    /// Package version.
    pub version: PackageVersion,
}

/// Where a launch binding's identity comes from.
///
/// The variants never share fields: a built-in runtime has a descriptor
/// digest and no package digest, a package runtime has a package digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "BindingProvenance.ts"))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BindingProvenance {
    /// A runtime compiled into the daemon.
    Builtin {
        /// Package identity the built-in descriptor stands for, when it has
        /// one. The shell has none. Id and version are both present or both
        /// absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        package: Option<PackageIdentity>,
        /// Digest of the descriptor's structural launch fields; not a package
        /// digest.
        descriptor_digest: DescriptorDigest,
    },
    /// A runtime installed from an authenticated package.
    Package {
        /// Package that exports the runtime.
        package: PackageIdentity,
        /// Digest of the installed package archive.
        package_digest: PackageDigest,
    },
}

/// The runtime identity a session was launched with.
///
/// It snapshots which runtime was selected and where its definition came
/// from, so a later registry change cannot silently re-point a recorded
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "LaunchBinding.ts"))]
#[serde(deny_unknown_fields)]
pub struct LaunchBinding {
    /// Selected runtime identity.
    pub runtime_id: RuntimeId,
    /// Origin of the runtime definition.
    pub provenance: BindingProvenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_A: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn accepts_valid_identifiers() {
        for value in ["shell", "codex", "a", "my-agent.v2_x", "0abc", "a.b.c"] {
            let id = RuntimeId::parse(value).expect(value);
            assert_eq!(id.as_str(), value);
            assert_eq!(id.to_string(), value);
        }
        let max = "a".repeat(MAX_AGENT_RUNTIME_ID_BYTES);
        RuntimeId::parse(&max).expect("must be valid");
    }

    #[test]
    fn rejects_invalid_identifiers() {
        let cases: [(&str, RuntimeIdError); 8] = [
            (
                "",
                RuntimeIdError::Empty {
                    type_name: "RuntimeId",
                },
            ),
            (
                "Codex",
                RuntimeIdError::InvalidCharacter {
                    type_name: "RuntimeId",
                    index: 0,
                },
            ),
            (
                "a b",
                RuntimeIdError::InvalidCharacter {
                    type_name: "RuntimeId",
                    index: 1,
                },
            ),
            (
                "a/b",
                RuntimeIdError::InvalidCharacter {
                    type_name: "RuntimeId",
                    index: 1,
                },
            ),
            (
                "a\u{e9}",
                RuntimeIdError::InvalidCharacter {
                    type_name: "RuntimeId",
                    index: 1,
                },
            ),
            (
                ".hidden",
                RuntimeIdError::LeadingSeparator {
                    type_name: "RuntimeId",
                },
            ),
            (
                "-flag",
                RuntimeIdError::LeadingSeparator {
                    type_name: "RuntimeId",
                },
            ),
            (
                "a..b",
                RuntimeIdError::ConsecutiveDots {
                    type_name: "RuntimeId",
                },
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(RuntimeId::parse(value), Err(expected), "{value:?}");
        }
    }

    #[test]
    fn rejects_over_length_identifiers() {
        let over = "a".repeat(MAX_AGENT_RUNTIME_ID_BYTES + 1);
        assert_eq!(
            RuntimeId::parse(&over),
            Err(RuntimeIdError::TooLong {
                type_name: "RuntimeId",
                max: MAX_AGENT_RUNTIME_ID_BYTES
            })
        );
        let package = "a".repeat(MAX_PACKAGE_ID_BYTES + 1);
        assert!(matches!(
            PackageId::parse(&package),
            Err(RuntimeIdError::TooLong { .. })
        ));
        PackageId::parse(&"a".repeat(MAX_PACKAGE_ID_BYTES)).expect("must be valid");
    }

    #[test]
    fn diagnostics_never_echo_the_rejected_text() {
        let error = RuntimeId::parse("secret value").expect_err("space is invalid");
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn trusted_builtin_literals_are_valid() {
        for value in ["shell", "codex", "claude", "hermes"] {
            assert_eq!(
                RuntimeId::from_trusted(value),
                RuntimeId::parse(value).unwrap()
            );
        }
        assert_eq!(RuntimeId::SHELL, "shell");
    }

    #[test]
    fn runtime_id_deserialization_is_strict() {
        let id: RuntimeId = serde_json::from_str(r#""codex""#).expect("valid");
        assert_eq!(serde_json::to_string(&id).expect("serialize"), r#""codex""#);
        for bad in [r#""""#, r#""Codex""#, r#""a..b""#, "1", "null"] {
            serde_json::from_str::<RuntimeId>(bad).expect_err(bad);
        }
    }

    #[test]
    fn historical_references_are_inert_and_displayable() {
        let historical: RuntimeRef = serde_json::from_str(r#""Legacy Agent!""#).expect("lenient");
        assert_eq!(historical, RuntimeRef::from_wire("Legacy Agent!"));
        assert_eq!(historical.to_string(), "Legacy Agent!");
        let error = historical.launchable().expect_err("never launchable");
        assert_eq!(error.code, "agent_kind_unsupported");
        assert!(!error.msg.contains("Legacy"));

        let hostile = RuntimeRef::from_wire("line\nbreak\u{1b}[31m secret");
        let error = hostile.launchable().expect_err("never launchable");
        assert!(!error.msg.contains('\n') && !error.msg.contains('\u{1b}'));
        assert!(!error.msg.contains("secret"));
        assert!(!error.recover.unwrap_or_default().contains("secret"));
        assert_eq!(
            serde_json::to_string(&historical).expect("serialize"),
            r#""Legacy Agent!""#
        );

        let empty: RuntimeRef = serde_json::from_str(r#""""#).expect("lenient");
        empty.launchable().expect_err("must be rejected");
        let over = "a".repeat(MAX_AGENT_RUNTIME_ID_BYTES + 1);
        RuntimeRef::from_wire(&over)
            .launchable()
            .expect_err("must be rejected");
    }

    #[test]
    fn historical_runtime_cannot_hold_a_valid_id() {
        for valid in ["codex", "shell", "future-agent", "a"] {
            assert_eq!(HistoricalRuntime::new(valid), None, "{valid}");
        }
        for invalid in ["", "Codex", "a b", "a..b", ".x"] {
            let label = HistoricalRuntime::new(invalid).expect(invalid);
            assert_eq!(label.as_str(), invalid);
            assert_eq!(label.to_string(), invalid);
            let reference = RuntimeRef::Historical(label);
            let json = serde_json::to_string(&reference).expect("serialize");
            let back: RuntimeRef = serde_json::from_str(&json).expect("roundtrip");
            assert_eq!(back, reference);
            assert!(matches!(back, RuntimeRef::Historical(_)));
        }
    }

    #[test]
    fn valid_references_are_launchable_and_roundtrip() {
        let reference: RuntimeRef = serde_json::from_str(r#""claude""#).expect("valid");
        assert_eq!(
            reference.launchable().expect("launchable").as_str(),
            "claude"
        );
        assert_eq!(
            serde_json::to_string(&reference).expect("serialize"),
            r#""claude""#
        );
        assert_eq!(
            RuntimeRef::from(RuntimeId::parse("claude").unwrap()),
            reference
        );
    }

    #[test]
    fn agent_kind_maps_to_runtime_refs_without_launching_unknown_values() {
        use crate::AgentKind;

        for kind in [
            AgentKind::Shell,
            AgentKind::Codex,
            AgentKind::Claude,
            AgentKind::Hermes,
        ] {
            let reference = kind.as_runtime_ref();
            assert_eq!(reference.as_wire(), kind.as_wire());
            assert_eq!(
                reference.launchable().expect("built-in is launchable"),
                &RuntimeId::parse(kind.as_wire()).expect("valid id")
            );
        }
        let unknown = AgentKind::Unknown("future-agent".to_owned()).as_runtime_ref();
        assert_eq!(
            unknown,
            RuntimeRef::Id(RuntimeId::parse("future-agent").unwrap())
        );
        // The wire form is a bare string, so the reference survives a roundtrip.
        let json = serde_json::to_string(&unknown).expect("serialize");
        assert_eq!(
            serde_json::from_str::<RuntimeRef>(&json).expect("roundtrip"),
            unknown
        );
        // Without a registry, launchable() accepts a grammar-valid id; the
        // registry then answers `runtime_not_installed`.
        assert_eq!(
            unknown.launchable().expect("grammar-valid").as_str(),
            "future-agent"
        );

        let invalid = AgentKind::Unknown("Not Valid".to_owned()).as_runtime_ref();
        assert_eq!(invalid, RuntimeRef::from_wire("Not Valid"));
        assert!(matches!(invalid, RuntimeRef::Historical(_)));
        let json = serde_json::to_string(&invalid).expect("serialize");
        assert_eq!(
            serde_json::from_str::<RuntimeRef>(&json).expect("roundtrip"),
            invalid
        );
    }

    #[test]
    fn runtime_not_installed_error_is_stable() {
        let error = ProtocolError::runtime_not_installed(&RuntimeId::parse("acme").unwrap());
        assert_eq!(error.class, crate::ErrorClass::Runtime);
        assert_eq!(error.code, "runtime_not_installed");
        assert!(error.msg.contains("`acme`"));
        assert!(error.recover.is_some());
        let json = serde_json::to_string(&error).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ProtocolError>(&json).expect("roundtrip"),
            error
        );
    }

    #[test]
    fn version_and_digest_validation() {
        PackageVersion::parse("1.0.0").expect("must be valid");
        PackageVersion::parse("1.0.0+build_7-rc").expect("must be valid");
        for bad in ["", "1 0", "1/0"] {
            assert_eq!(PackageVersion::parse(bad), Err(BindingFieldError::Version));
        }
        PackageVersion::parse(&"1".repeat(MAX_PACKAGE_VERSION_BYTES + 1))
            .expect_err("must be rejected");

        PackageDigest::parse(DIGEST_A).expect("must be valid");
        for bad in [
            "",
            "sha256:",
            "sha256:00",
            "sha512:0000000000000000000000000000000000000000000000000000000000000000",
            "sha256:000000000000000000000000000000000000000000000000000000000000000G",
            "sha256:000000000000000000000000000000000000000000000000000000000000000A",
        ] {
            assert_eq!(
                DescriptorDigest::parse(bad),
                Err(BindingFieldError::Digest),
                "{bad}"
            );
        }
    }

    #[test]
    fn launch_binding_roundtrips_both_provenances() {
        let builtin = LaunchBinding {
            runtime_id: RuntimeId::parse("codex").unwrap(),
            provenance: BindingProvenance::Builtin {
                package: Some(PackageIdentity {
                    id: PackageId::parse("pohunek.runtime.codex").unwrap(),
                    version: PackageVersion::parse("1.0.0").unwrap(),
                }),
                descriptor_digest: DescriptorDigest::parse(DIGEST_A).unwrap(),
            },
        };
        let json = serde_json::to_value(&builtin).expect("serialize");
        assert_eq!(json["provenance"]["kind"], "builtin");
        assert!(json["provenance"].get("package_digest").is_none());
        assert_eq!(
            serde_json::from_value::<LaunchBinding>(json).expect("roundtrip"),
            builtin
        );

        let shell = LaunchBinding {
            runtime_id: RuntimeId::parse("shell").unwrap(),
            provenance: BindingProvenance::Builtin {
                package: None,
                descriptor_digest: DescriptorDigest::parse(DIGEST_A).unwrap(),
            },
        };
        let json = serde_json::to_value(&shell).expect("serialize");
        assert!(json["provenance"].get("package").is_none());
        assert_eq!(
            serde_json::from_value::<LaunchBinding>(json).expect("roundtrip"),
            shell
        );

        let package = LaunchBinding {
            runtime_id: RuntimeId::parse("acme").unwrap(),
            provenance: BindingProvenance::Package {
                package: PackageIdentity {
                    id: PackageId::parse("acme.runtime").unwrap(),
                    version: PackageVersion::parse("2.1.0").unwrap(),
                },
                package_digest: PackageDigest::parse(DIGEST_A).unwrap(),
            },
        };
        let json = serde_json::to_value(&package).expect("serialize");
        assert_eq!(json["provenance"]["kind"], "package");
        assert_eq!(
            serde_json::from_value::<LaunchBinding>(json).expect("roundtrip"),
            package
        );
    }

    #[test]
    fn launch_binding_rejects_unknown_and_fabricated_fields() {
        let builtin_with_package_digest = serde_json::json!({
            "runtime_id": "codex",
            "provenance": {
                "kind": "builtin",
                "descriptor_digest": DIGEST_A,
                "package_digest": DIGEST_A,
            }
        });
        serde_json::from_value::<LaunchBinding>(builtin_with_package_digest)
            .expect_err("must be rejected");

        let package_without_digest = serde_json::json!({
            "runtime_id": "acme",
            "provenance": {
                "kind": "package",
                "package": {"id": "acme.runtime", "version": "1.0.0"},
            }
        });
        serde_json::from_value::<LaunchBinding>(package_without_digest)
            .expect_err("must be rejected");

        // Id without version, version without id, and the old flat spelling
        // are all malformed.
        for package in [
            serde_json::json!({"id": "pohunek.runtime.codex"}),
            serde_json::json!({"version": "1.0.0"}),
        ] {
            let half_package = serde_json::json!({
                "runtime_id": "codex",
                "provenance": {
                    "kind": "builtin",
                    "package": package,
                    "descriptor_digest": DIGEST_A,
                }
            });
            serde_json::from_value::<LaunchBinding>(half_package).expect_err("must be rejected");
        }
        let flat = serde_json::json!({
            "runtime_id": "codex",
            "provenance": {
                "kind": "builtin",
                "package_id": "pohunek.runtime.codex",
                "descriptor_digest": DIGEST_A,
            }
        });
        serde_json::from_value::<LaunchBinding>(flat).expect_err("must be rejected");

        let extra = serde_json::json!({
            "runtime_id": "codex",
            "provenance": {"kind": "builtin", "descriptor_digest": DIGEST_A},
            "extra": 1,
        });
        serde_json::from_value::<LaunchBinding>(extra).expect_err("must be rejected");
    }
}
