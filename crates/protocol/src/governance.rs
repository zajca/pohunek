//! Stable host identity and local governance wire contracts.
//!
//! These types carry only opaque identifiers and safe owner inspection state.
//! Private approval keys and relay credentials never cross this contract.
//! Every governance identifier is `<lowercase type tag>_` followed by the
//! canonical unpadded base64url encoding of exactly 32 bytes.

use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use zeroize::Zeroizing;

use crate::{
    ED25519_SIGNATURE_BYTES, ED25519_SIGNATURE_PAYLOAD_BYTES, GOVERNANCE_ID_PAYLOAD_BYTES,
};

const BASE64URL_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const TRANSFER_OUTCOME_SIGNING_DOMAIN: &[u8] = b"pohunek.host-approval.transfer-outcome.v1\0";
const PRINCIPAL_OWNER_TAG: u8 = 1;
const TEAM_OWNER_TAG: u8 = 2;
const ALL_ACTIVE_SHARES_TAG: u8 = 1;
const ED25519_APPROVAL_ALGORITHM_TAG: u8 = 1;

/// Reports malformed governance wire values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GovernanceWireError {
    /// A value did not carry the required canonical type prefix.
    #[error("{type_name} must begin with `{prefix}`")]
    Prefix {
        /// Human-readable type name.
        type_name: &'static str,
        /// Required wire prefix.
        prefix: &'static str,
    },
    /// A value did not have the required fixed payload length.
    #[error("{type_name} must contain exactly {expected} payload characters")]
    Length {
        /// Human-readable type name.
        type_name: &'static str,
        /// Required payload length.
        expected: usize,
    },
    /// A value contained a character outside its canonical alphabet.
    #[error("{type_name} must use its canonical payload alphabet")]
    Alphabet {
        /// Human-readable type name.
        type_name: &'static str,
    },
    /// A revision was zero.
    #[error("{type_name} must be nonzero")]
    Zero {
        /// Human-readable type name.
        type_name: &'static str,
    },
    /// A decimal revision was not canonical or exceeded `u64`.
    #[error("{type_name} must be a canonical unsigned decimal string")]
    Decimal {
        /// Human-readable type name.
        type_name: &'static str,
    },
    /// A revision cannot advance past `u64::MAX`.
    #[error("{type_name} cannot advance past its maximum revision")]
    RevisionOverflow {
        /// Human-readable type name.
        type_name: &'static str,
    },
    /// An RFC3339 expiry was malformed or noncanonical.
    #[error("proposal expiry must be a canonical RFC3339 timestamp")]
    Expiry,
    /// A signature payload was not one canonical Ed25519 proof.
    #[error("transfer signature must be one canonical Ed25519 proof")]
    Signature,
    /// A status represented impossible owner/enrollment coordinates.
    #[error("host governance status has inconsistent owner or quarantine state")]
    Status,
}

fn parse_identifier(
    value: &str,
    type_name: &'static str,
    prefix: &'static str,
) -> Result<String, GovernanceWireError> {
    let payload = value
        .strip_prefix(prefix)
        .ok_or(GovernanceWireError::Prefix { type_name, prefix })?;
    if payload.len() != GOVERNANCE_ID_PAYLOAD_BYTES {
        return Err(GovernanceWireError::Length {
            type_name,
            expected: GOVERNANCE_ID_PAYLOAD_BYTES,
        });
    }
    if !payload
        .bytes()
        .all(|byte| BASE64URL_ALPHABET.as_bytes().contains(&byte))
    {
        return Err(GovernanceWireError::Alphabet { type_name });
    }
    let decoded = BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_error| GovernanceWireError::Alphabet { type_name })?;
    if decoded.len() != 32 || BASE64_URL_SAFE_NO_PAD.encode(decoded) != payload {
        return Err(GovernanceWireError::Alphabet { type_name });
    }
    Ok(value.to_owned())
}

macro_rules! identifier_type {
    ($(#[$docs:meta])* $name:ident, $prefix:literal $(, #[$ts:meta])*) => {
        $(#[$docs])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $(#[$ts])*
        pub struct $name(String);

        impl $name {
            /// Parses one canonical identifier.
            ///
            /// # Errors
            ///
            /// Returns [`GovernanceWireError`] when the prefix, fixed payload
            /// length, or canonical unpadded base64url payload is invalid.
            pub fn parse(value: &str) -> Result<Self, GovernanceWireError> {
                parse_identifier(value, stringify!($name), $prefix).map(Self)
            }

            /// Returns the canonical wire representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<&str> for $name {
            type Error = GovernanceWireError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Debug for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
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

identifier_type!(
    /// Stable identity generated once by one host.
    HostId,
    "host_",
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))],
    #[cfg_attr(feature = "ts", ts(export, export_to = "HostId.ts", type = "string"))]
);
identifier_type!(
    /// Stable identity of one relay deployment.
    RelayId,
    "relay_",
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))],
    #[cfg_attr(feature = "ts", ts(export, export_to = "RelayId.ts", type = "string"))]
);
identifier_type!(
    /// Opaque relay principal identity.
    PrincipalId,
    "principal_",
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))],
    #[cfg_attr(feature = "ts", ts(export, export_to = "PrincipalId.ts", type = "string"))]
);
identifier_type!(
    /// Opaque relay team identity.
    TeamId,
    "team_",
    #[cfg_attr(feature = "ts", derive(ts_rs::TS))],
    #[cfg_attr(feature = "ts", ts(export, export_to = "TeamId.ts", type = "string"))]
);
identifier_type!(
    /// One-use local ownership-transfer proposal identity.
    ProposalId,
    "proposal_"
);
identifier_type!(
    /// Deterministic durable outcome identity derived from a proposal.
    TransferOutcomeId,
    "outcome_"
);

impl TransferOutcomeId {
    /// Derives the unique durable outcome identity for one proposal.
    ///
    /// The derived identity preserves the proposal's exact canonical 32-byte
    /// payload and replaces only its `proposal_` type tag with `outcome_`.
    /// Therefore one proposal has exactly one outcome identity, while the two
    /// strong types remain non-interchangeable at every wire boundary.
    #[must_use]
    pub fn from_proposal_id(proposal_id: &ProposalId) -> Self {
        let payload = proposal_id
            .0
            .strip_prefix("proposal_")
            .expect("ProposalId always stores the validated proposal_ type tag");
        Self(format!("outcome_{payload}"))
    }
}

/// Safe reference containing an Ed25519 verifying-key payload.
///
/// Its canonical `approval_key_` payload is exactly one raw 32-byte candidate
/// Ed25519 verifying-key encoding. It is neither a handle nor a hash, so
/// durable host state can restore this reference and a future relay verifier
/// can use its bytes directly. This protocol type validates only canonical
/// wire syntax and length; the daemon's B2 cryptographic backend must validate
/// mathematical Ed25519 key validity before use. This type never carries a
/// signing seed or other private key material.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "ApprovalKeyReference.ts", type = "string")
)]
pub struct ApprovalKeyReference {
    value: String,
    verifying_key: [u8; 32],
}

impl ApprovalKeyReference {
    /// Parses one canonical Ed25519 approval-key payload.
    ///
    /// # Errors
    ///
    /// Returns [`GovernanceWireError`] when the prefix, fixed payload length,
    /// or canonical unpadded base64url payload is invalid.
    pub fn parse(value: &str) -> Result<Self, GovernanceWireError> {
        let value = parse_identifier(value, "ApprovalKeyReference", "approval_key_")?;
        let payload = value
            .strip_prefix("approval_key_")
            .ok_or(GovernanceWireError::Prefix {
                type_name: "ApprovalKeyReference",
                prefix: "approval_key_",
            })?;
        let decoded = BASE64_URL_SAFE_NO_PAD.decode(payload).map_err(|_error| {
            GovernanceWireError::Alphabet {
                type_name: "ApprovalKeyReference",
            }
        })?;
        let verifying_key = decoded
            .try_into()
            .map_err(|_error| GovernanceWireError::Length {
                type_name: "ApprovalKeyReference",
                expected: GOVERNANCE_ID_PAYLOAD_BYTES,
            })?;
        Ok(Self {
            value,
            verifying_key,
        })
    }

    /// Creates a reference from one raw Ed25519 verifying-key payload.
    ///
    /// This conversion preserves all 32 bytes without cryptographic validation.
    /// The daemon's B2 cryptographic backend must validate mathematical key
    /// validity before using the payload for Ed25519 verification.
    #[must_use]
    pub fn from_ed25519_verifying_key_bytes(verifying_key: [u8; 32]) -> Self {
        Self {
            value: format!(
                "approval_key_{}",
                BASE64_URL_SAFE_NO_PAD.encode(verifying_key)
            ),
            verifying_key,
        }
    }

    /// Returns the raw 32-byte candidate Ed25519 verifying-key encoding.
    ///
    /// The returned bytes have canonical wire syntax and length only. The
    /// daemon's B2 cryptographic backend must validate mathematical key
    /// validity before use.
    #[must_use]
    pub const fn ed25519_verifying_key_bytes(&self) -> [u8; 32] {
        self.verifying_key
    }

    /// Returns the canonical wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl TryFrom<&str> for ApprovalKeyReference {
    type Error = GovernanceWireError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Display for ApprovalKeyReference {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value)
    }
}

impl Debug for ApprovalKeyReference {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ApprovalKeyReference")
            .field(&self.value)
            .finish()
    }
}

impl Serialize for ApprovalKeyReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ApprovalKeyReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// One-use secret coordinate carried by an ownership transfer proposal.
#[derive(Clone, PartialEq, Eq)]
pub struct ProposalNonce(Zeroizing<String>);

fn parse_owned_proposal_nonce(
    value: Zeroizing<String>,
) -> Result<ProposalNonce, GovernanceWireError> {
    {
        let payload = value
            .strip_prefix("nonce_")
            .ok_or(GovernanceWireError::Prefix {
                type_name: "ProposalNonce",
                prefix: "nonce_",
            })?;
        if payload.len() != GOVERNANCE_ID_PAYLOAD_BYTES {
            return Err(GovernanceWireError::Length {
                type_name: "ProposalNonce",
                expected: GOVERNANCE_ID_PAYLOAD_BYTES,
            });
        }
        if !payload
            .bytes()
            .all(|byte| BASE64URL_ALPHABET.as_bytes().contains(&byte))
        {
            return Err(GovernanceWireError::Alphabet {
                type_name: "ProposalNonce",
            });
        }

        let decoded = Zeroizing::new(BASE64_URL_SAFE_NO_PAD.decode(payload).map_err(|_error| {
            GovernanceWireError::Alphabet {
                type_name: "ProposalNonce",
            }
        })?);
        let canonical = Zeroizing::new(BASE64_URL_SAFE_NO_PAD.encode(decoded.as_slice()));
        if decoded.len() != 32 || canonical.as_str() != payload {
            return Err(GovernanceWireError::Alphabet {
                type_name: "ProposalNonce",
            });
        }
    }

    Ok(ProposalNonce(value))
}

impl ProposalNonce {
    /// Parses one canonical proposal nonce.
    ///
    /// # Errors
    ///
    /// Returns [`GovernanceWireError`] when the prefix, fixed payload length, or
    /// canonical unpadded base64url payload is invalid.
    ///
    /// The canonical string owned by this type, including cloned nonce values,
    /// is zeroized on drop. Parsing and deserialization also zeroize their
    /// owned input, decoded, and canonicality scratch buffers before drop.
    /// This does not extend to caller-owned input or serializer-owned output
    /// buffers outside this type's ownership.
    pub fn parse(value: &str) -> Result<Self, GovernanceWireError> {
        parse_owned_proposal_nonce(Zeroizing::new(value.to_owned()))
    }
}

impl TryFrom<&str> for ProposalNonce {
    type Error = GovernanceWireError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Debug for ProposalNonce {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProposalNonce([REDACTED])")
    }
}

impl Hash for ProposalNonce {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl Serialize for ProposalNonce {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for ProposalNonce {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProposalNonceVisitor;

        impl Visitor<'_> for ProposalNonceVisitor {
            type Value = ProposalNonce;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a canonical proposal nonce string")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                parse_owned_proposal_nonce(Zeroizing::new(v.to_owned())).map_err(E::custom)
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                parse_owned_proposal_nonce(Zeroizing::new(v)).map_err(E::custom)
            }
        }

        deserializer.deserialize_string(ProposalNonceVisitor)
    }
}

#[cfg(test)]
mod proposal_nonce_tests {
    use super::*;

    #[test]
    fn proposal_nonce_stores_its_canonical_string_in_zeroizing_memory() {
        fn assert_zeroizing_string(_: &Zeroizing<String>) {}

        let nonce = ProposalNonce::parse("nonce_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .expect("canonical nonce");
        assert_zeroizing_string(&nonce.0);
        assert_eq!(format!("{nonce:?}"), "ProposalNonce([REDACTED])");
    }
}

macro_rules! revision_type {
    ($(#[$docs:meta])* $name:ident, $export:literal) => {
        $(#[$docs])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        #[cfg_attr(feature = "ts", ts(export, export_to = $export, type = "string"))]
        pub struct $name(u64);

        impl $name {
            /// Creates a nonzero revision.
            ///
            /// # Errors
            ///
            /// Returns [`GovernanceWireError::Zero`] when `value` is zero.
            pub const fn new(value: u64) -> Result<Self, GovernanceWireError> {
                if value == 0 {
                    Err(GovernanceWireError::Zero {
                        type_name: stringify!($name),
                    })
                } else {
                    Ok(Self(value))
                }
            }

            /// Parses a canonical nonzero decimal revision.
            ///
            /// # Errors
            ///
            /// Returns [`GovernanceWireError`] for zero, noncanonical decimal,
            /// or an out-of-range integer.
            pub fn parse(value: &str) -> Result<Self, GovernanceWireError> {
                let parsed = value.parse::<u64>().map_err(|_error| GovernanceWireError::Decimal {
                    type_name: stringify!($name),
                })?;
                if parsed.to_string() != value {
                    return Err(GovernanceWireError::Decimal {
                        type_name: stringify!($name),
                    });
                }
                Self::new(parsed)
            }

            /// Returns the exact revision value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Advances this revision without wrapping.
            ///
            /// # Errors
            ///
            /// Returns [`GovernanceWireError::RevisionOverflow`] at `u64::MAX`.
            pub const fn checked_next(self) -> Result<Self, GovernanceWireError> {
                match self.0.checked_add(1) {
                    Some(value) => Ok(Self(value)),
                    None => Err(GovernanceWireError::RevisionOverflow {
                        type_name: stringify!($name),
                    }),
                }
            }
        }

        impl TryFrom<&str> for $name {
            type Error = GovernanceWireError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                Display::fmt(&self.0, f)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
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

revision_type!(
    /// Monotonic revision of one enrolled host owner record.
    OwnerRevision,
    "OwnerRevision.ts"
);
revision_type!(
    /// Monotonic revision of one host enrollment record.
    EnrollmentRevision,
    "EnrollmentRevision.ts"
);

/// Kind of the exact registered host owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostOwnerKind.ts"))]
#[serde(rename_all = "snake_case")]
pub enum HostOwnerKind {
    /// A single relay principal owns the host.
    Principal,
    /// A single relay team owns the host.
    Team,
}

/// Exact registered host owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostOwner.ts"))]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum HostOwner {
    /// One exact relay principal.
    Principal(PrincipalId),
    /// One exact relay team.
    Team(TeamId),
}

impl<'de> Deserialize<'de> for HostOwner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(
            tag = "kind",
            content = "id",
            rename_all = "snake_case",
            deny_unknown_fields
        )]
        enum WireOwner {
            Principal(PrincipalId),
            Team(TeamId),
        }

        match WireOwner::deserialize(deserializer)? {
            WireOwner::Principal(id) => Ok(Self::Principal(id)),
            WireOwner::Team(id) => Ok(Self::Team(id)),
        }
    }
}

impl HostOwner {
    /// Returns the owner kind.
    #[must_use]
    pub const fn kind(&self) -> HostOwnerKind {
        match self {
            Self::Principal(_) => HostOwnerKind::Principal,
            Self::Team(_) => HostOwnerKind::Team,
        }
    }
}

/// Durable lifecycle state for a known relay enrollment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "EnrollmentStatus.ts"))]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentStatus {
    /// Relay use is disabled but historical enrollment coordinates remain known.
    Disabled,
    /// Local durable commit is pending confirmation.
    PendingLocalCommit,
    /// One relay enrollment is active.
    Active,
    /// Locally confirmed relay rotation is in progress.
    Rotating,
    /// Governance and share mutation is quarantined.
    Quarantined,
    /// The local operator explicitly unenrolled the host.
    LocallyUnenrolled,
}

/// Safe reason why relay governance is quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "QuarantineReason.ts"))]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    /// More than one process claimed the stable host identity.
    HostIdentityClone,
    /// The relay projection disagreed with the authoritative host record.
    ProjectionConflict,
    /// A conflicting enrollment state was detected locally.
    EnrollmentConflict,
}

/// One known relay enrollment's safe public state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "EnrollmentInfo.ts"))]
pub struct EnrollmentInfo {
    relay_id: RelayId,
    status: EnrollmentStatus,
    revision: EnrollmentRevision,
}

impl<'de> Deserialize<'de> for EnrollmentInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEnrollmentInfo {
            relay_id: RelayId,
            status: EnrollmentStatus,
            revision: EnrollmentRevision,
        }

        let wire = WireEnrollmentInfo::deserialize(deserializer)?;
        Ok(Self::new(wire.relay_id, wire.status, wire.revision))
    }
}

impl EnrollmentInfo {
    /// Creates one revisioned enrollment state.
    #[must_use]
    pub const fn new(
        relay_id: RelayId,
        status: EnrollmentStatus,
        revision: EnrollmentRevision,
    ) -> Self {
        Self {
            relay_id,
            status,
            revision,
        }
    }

    /// Returns the enrolled relay identity.
    #[must_use]
    pub const fn relay_id(&self) -> &RelayId {
        &self.relay_id
    }

    /// Returns the durable enrollment state.
    #[must_use]
    pub const fn status(&self) -> EnrollmentStatus {
        self.status
    }

    /// Returns the enrollment revision.
    #[must_use]
    pub const fn revision(&self) -> EnrollmentRevision {
        self.revision
    }
}

/// Safe owner-only governance inspection result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostGovernanceStatus.ts"))]
pub struct HostGovernanceStatus {
    host_id: HostId,
    enrollment: Option<EnrollmentInfo>,
    owner: Option<HostOwner>,
    owner_revision: Option<OwnerRevision>,
    quarantine: Option<QuarantineReason>,
    approval_key_reference: ApprovalKeyReference,
}

impl HostGovernanceStatus {
    /// Creates a safe snapshot of local governance state.
    ///
    /// The approval-key reference is required because bootstrap creates the host
    /// identity and approval key before any governance state is inspectable.
    ///
    /// # Errors
    ///
    /// Returns [`GovernanceWireError::Status`] when owner, enrollment, and
    /// owner revision do not occur together, or quarantine does not match a
    /// quarantined enrollment.
    pub fn new(
        host_id: HostId,
        enrollment: Option<EnrollmentInfo>,
        owner: Option<HostOwner>,
        owner_revision: Option<OwnerRevision>,
        quarantine: Option<QuarantineReason>,
        approval_key_reference: ApprovalKeyReference,
    ) -> Result<Self, GovernanceWireError> {
        let status = Self {
            host_id,
            enrollment,
            owner,
            owner_revision,
            quarantine,
            approval_key_reference,
        };
        status.validate()?;
        Ok(status)
    }

    fn validate(&self) -> Result<(), GovernanceWireError> {
        if self.enrollment.is_some() != self.owner.is_some() {
            return Err(GovernanceWireError::Status);
        }
        if self.enrollment.is_some() != self.owner_revision.is_some() {
            return Err(GovernanceWireError::Status);
        }
        let is_quarantined = self
            .enrollment
            .as_ref()
            .is_some_and(|enrollment| enrollment.status == EnrollmentStatus::Quarantined);
        if self.quarantine.is_some() != is_quarantined {
            return Err(GovernanceWireError::Status);
        }
        Ok(())
    }

    /// Returns the stable host identity.
    #[must_use]
    pub const fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// Returns the known relay enrollment, if the host has ever enrolled.
    #[must_use]
    pub const fn enrollment(&self) -> Option<&EnrollmentInfo> {
        self.enrollment.as_ref()
    }

    /// Returns the exact registered owner, if the host has enrolled.
    #[must_use]
    pub const fn owner(&self) -> Option<&HostOwner> {
        self.owner.as_ref()
    }

    /// Returns the current owner revision, if governance is present.
    #[must_use]
    pub const fn owner_revision(&self) -> Option<OwnerRevision> {
        self.owner_revision
    }

    /// Returns the relay-governance quarantine reason, if any.
    #[must_use]
    pub const fn quarantine(&self) -> Option<QuarantineReason> {
        self.quarantine
    }

    /// Returns the safe reference to initialized approval-key material.
    #[must_use]
    pub const fn approval_key_reference(&self) -> &ApprovalKeyReference {
        &self.approval_key_reference
    }
}

impl<'de> Deserialize<'de> for HostGovernanceStatus {
    #[expect(
        clippy::too_many_lines,
        reason = "Strict manual field-presence and duplicate-field tracking keeps the public status schema auditable in one place."
    )]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StatusVisitor;

        impl<'de> Visitor<'de> for StatusVisitor {
            type Value = HostGovernanceStatus;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a host governance status object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut host_id = None;
                let mut enrollment = None;
                let mut owner = None;
                let mut owner_revision = None;
                let mut quarantine = None;
                let mut approval_key_reference = None;

                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "host_id" => {
                            if host_id.is_some() {
                                return Err(serde::de::Error::duplicate_field("host_id"));
                            }
                            host_id = Some(map.next_value()?);
                        }
                        "enrollment" => {
                            if enrollment.is_some() {
                                return Err(serde::de::Error::duplicate_field("enrollment"));
                            }
                            enrollment = Some(map.next_value()?);
                        }
                        "owner" => {
                            if owner.is_some() {
                                return Err(serde::de::Error::duplicate_field("owner"));
                            }
                            owner = Some(map.next_value()?);
                        }
                        "owner_revision" => {
                            if owner_revision.is_some() {
                                return Err(serde::de::Error::duplicate_field("owner_revision"));
                            }
                            owner_revision = Some(map.next_value()?);
                        }
                        "quarantine" => {
                            if quarantine.is_some() {
                                return Err(serde::de::Error::duplicate_field("quarantine"));
                            }
                            quarantine = Some(map.next_value()?);
                        }
                        "approval_key_reference" => {
                            if approval_key_reference.is_some() {
                                return Err(serde::de::Error::duplicate_field(
                                    "approval_key_reference",
                                ));
                            }
                            approval_key_reference = Some(map.next_value()?);
                        }
                        _ => {
                            return Err(serde::de::Error::unknown_field(
                                &field,
                                &[
                                    "host_id",
                                    "enrollment",
                                    "owner",
                                    "owner_revision",
                                    "quarantine",
                                    "approval_key_reference",
                                ],
                            ));
                        }
                    }
                }

                let host_id = host_id.ok_or_else(|| serde::de::Error::missing_field("host_id"))?;
                let enrollment =
                    enrollment.ok_or_else(|| serde::de::Error::missing_field("enrollment"))?;
                let owner = owner.ok_or_else(|| serde::de::Error::missing_field("owner"))?;
                let approval_key_reference = approval_key_reference
                    .ok_or_else(|| serde::de::Error::missing_field("approval_key_reference"))?;
                let owner_revision = owner_revision
                    .ok_or_else(|| serde::de::Error::missing_field("owner_revision"))?;
                let quarantine =
                    quarantine.ok_or_else(|| serde::de::Error::missing_field("quarantine"))?;

                Self::Value::new(
                    host_id,
                    enrollment,
                    owner,
                    owner_revision,
                    quarantine,
                    approval_key_reference,
                )
                .map_err(serde::de::Error::custom)
            }
        }

        const FIELDS: &[&str] = &[
            "host_id",
            "enrollment",
            "owner",
            "owner_revision",
            "quarantine",
            "approval_key_reference",
        ];
        deserializer.deserialize_struct("HostGovernanceStatus", FIELDS, StatusVisitor)
    }
}

/// Canonical UTC expiry bound to one ownership transfer proposal.
///
/// The wire spelling is `YYYY-MM-DDTHH:MM:SSZ` or
/// `YYYY-MM-DDTHH:MM:SS.<fraction>Z`, with uppercase `T` and `Z`. The fraction
/// is optional, has one through nine decimal digits, represents nonzero
/// nanoseconds, and has no trailing zero. Numeric date and time components use
/// the validation and canonical rendering of [`Rfc3339`]. Offsets, lowercase
/// UTC markers, redundant fractional zeros, and precision beyond nanoseconds
/// are not canonical wire values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProposalExpiry {
    value: String,
    instant: OffsetDateTime,
}

impl ProposalExpiry {
    /// Parses one canonical UTC RFC3339 expiry.
    ///
    /// # Errors
    ///
    /// Returns [`GovernanceWireError::Expiry`] for malformed or noncanonical
    /// timestamps.
    pub fn parse(value: &str) -> Result<Self, GovernanceWireError> {
        let parsed =
            OffsetDateTime::parse(value, &Rfc3339).map_err(|_error| GovernanceWireError::Expiry)?;
        let canonical = parsed
            .format(&Rfc3339)
            .map_err(|_error| GovernanceWireError::Expiry)?;
        if canonical != value {
            return Err(GovernanceWireError::Expiry);
        }
        Ok(Self {
            value: canonical,
            instant: parsed,
        })
    }

    /// Reports whether this proposal has expired at `now`.
    ///
    /// Expiry fails closed: the exact expiry instant is expired, so this
    /// returns `true` when `now >= expiry`. The comparison uses the instant
    /// validated while parsing and cannot reparse or fail.
    #[must_use]
    pub fn is_expired_at(&self, now: OffsetDateTime) -> bool {
        now >= self.instant
    }
}

impl TryFrom<&str> for ProposalExpiry {
    type Error = GovernanceWireError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Display for ProposalExpiry {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value)
    }
}

impl Serialize for ProposalExpiry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProposalExpiry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Exact Ed25519 host approval signature bytes.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct HostApprovalSignature(String);

impl HostApprovalSignature {
    /// Parses one canonical Ed25519 signature.
    ///
    /// # Errors
    ///
    /// Returns [`GovernanceWireError::Signature`] for a malformed, padded, or
    /// wrong-length signature. Verification remains the daemon's future concern.
    pub fn parse(value: &str) -> Result<Self, GovernanceWireError> {
        let Some(payload) = value.strip_prefix("sig_") else {
            return Err(GovernanceWireError::Signature);
        };
        if payload.len() != ED25519_SIGNATURE_PAYLOAD_BYTES
            || !payload
                .bytes()
                .all(|byte| BASE64URL_ALPHABET.as_bytes().contains(&byte))
        {
            return Err(GovernanceWireError::Signature);
        }
        let decoded = BASE64_URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_error| GovernanceWireError::Signature)?;
        if decoded.len() != ED25519_SIGNATURE_BYTES
            || BASE64_URL_SAFE_NO_PAD.encode(decoded) != payload
        {
            return Err(GovernanceWireError::Signature);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the canonical wire representation for signature verification.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for HostApprovalSignature {
    type Error = GovernanceWireError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Debug for HostApprovalSignature {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostApprovalSignature([REDACTED])")
    }
}

impl Serialize for HostApprovalSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HostApprovalSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Stable relay, host, and ownership binding for a transfer proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferCoordinates {
    relay_id: RelayId,
    host_id: HostId,
    owner_revision: OwnerRevision,
    current_owner: HostOwner,
    target: HostOwner,
}

impl TransferCoordinates {
    /// Creates one exact transfer binding.
    #[must_use]
    pub const fn new(
        relay_id: RelayId,
        host_id: HostId,
        owner_revision: OwnerRevision,
        current_owner: HostOwner,
        target: HostOwner,
    ) -> Self {
        Self {
            relay_id,
            host_id,
            owner_revision,
            current_owner,
            target,
        }
    }
}

/// Exact coordinate set that a local owner confirms for one transfer.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct TransferProposal {
    relay_id: RelayId,
    host_id: HostId,
    owner_revision: OwnerRevision,
    current_owner: HostOwner,
    target: HostOwner,
    proposal_id: ProposalId,
    nonce: ProposalNonce,
    expiry: ProposalExpiry,
}

impl TransferProposal {
    /// Creates a fully bound ownership-transfer proposal.
    #[must_use]
    pub fn new(
        coordinates: TransferCoordinates,
        proposal_id: ProposalId,
        nonce: ProposalNonce,
        expiry: ProposalExpiry,
    ) -> Self {
        Self {
            relay_id: coordinates.relay_id,
            host_id: coordinates.host_id,
            owner_revision: coordinates.owner_revision,
            current_owner: coordinates.current_owner,
            target: coordinates.target,
            proposal_id,
            nonce,
            expiry,
        }
    }

    /// Returns the relay this proposal is bound to.
    #[must_use]
    pub const fn relay_id(&self) -> &RelayId {
        &self.relay_id
    }

    /// Returns the host this proposal is bound to.
    #[must_use]
    pub const fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// Returns the owner revision that must still be current.
    #[must_use]
    pub const fn owner_revision(&self) -> OwnerRevision {
        self.owner_revision
    }

    /// Returns the exact target replacing the current owner.
    #[must_use]
    pub const fn target(&self) -> &HostOwner {
        &self.target
    }

    /// Returns the exact owner that the transfer replaces.
    #[must_use]
    pub const fn current_owner(&self) -> &HostOwner {
        &self.current_owner
    }

    /// Returns the proposal identifier.
    #[must_use]
    pub const fn proposal_id(&self) -> &ProposalId {
        &self.proposal_id
    }

    /// Returns whether another proposal carries the same secret one-use nonce.
    ///
    /// This narrow comparison seam supports local replay rejection against a
    /// retained proposal without exposing nonce bytes. It does not establish
    /// global nonce uniqueness, which remains the relay's responsibility.
    #[must_use]
    pub fn has_same_nonce(&self, other: &Self) -> bool {
        self.nonce == other.nonce
    }

    /// Returns the expiry timestamp.
    #[must_use]
    pub const fn expiry(&self) -> &ProposalExpiry {
        &self.expiry
    }
}

impl Debug for TransferProposal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferProposal")
            .field("relay_id", &self.relay_id)
            .field("host_id", &self.host_id)
            .field("owner_revision", &self.owner_revision)
            .field("current_owner", &self.current_owner)
            .field("target", &self.target)
            .field("proposal_id", &self.proposal_id)
            .field("nonce", &"[REDACTED]")
            .field("expiry", &self.expiry)
            .finish()
    }
}

impl<'de> Deserialize<'de> for TransferProposal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireProposal {
            relay_id: RelayId,
            host_id: HostId,
            owner_revision: OwnerRevision,
            current_owner: HostOwner,
            target: HostOwner,
            proposal_id: ProposalId,
            nonce: ProposalNonce,
            expiry: ProposalExpiry,
        }

        let wire = WireProposal::deserialize(deserializer)?;
        Ok(Self::new(
            TransferCoordinates::new(
                wire.relay_id,
                wire.host_id,
                wire.owner_revision,
                wire.current_owner,
                wire.target,
            ),
            wire.proposal_id,
            wire.nonce,
            wire.expiry,
        ))
    }
}

/// Reports inconsistent signed transfer outcomes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransferOutcomeError {
    /// The outcome identity did not derive from the proposal identity.
    #[error("transfer outcome identity must derive from its proposal identity")]
    OutcomeId,
    /// The recorded new owner revision did not advance the proposal revision once.
    #[error("signed transfer outcome must advance the owner revision exactly once")]
    Revision,
    /// A canonical signing field could not fit its fixed length prefix.
    #[error("transfer outcome signing field exceeds the supported wire length")]
    FieldLength,
}

/// Atomic intent to suspend every active share after an owner transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareSuspensionIntent {
    /// Suspend every active share before the transfer becomes visible.
    AllActiveShares,
}

/// Algorithm used by the host approval key for one signed transfer outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostApprovalAlgorithm {
    /// A 64-byte Ed25519 signature over the canonical outcome payload.
    Ed25519,
}

/// Candidate outcome prepared before host approval signing and persistence.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct TransferOutcomeCandidate {
    outcome_id: TransferOutcomeId,
    proposal: TransferProposal,
    new_owner_revision: OwnerRevision,
    suspension: ShareSuspensionIntent,
    #[serde(skip)]
    approval_algorithm: HostApprovalAlgorithm,
    approval_key_reference: ApprovalKeyReference,
}

impl TransferOutcomeCandidate {
    /// Creates an outcome candidate with one checked owner-revision advance.
    ///
    /// # Errors
    ///
    /// Returns [`TransferOutcomeError::OutcomeId`] when `outcome_id` does not
    /// derive from `proposal`, or [`TransferOutcomeError::Revision`] when the
    /// outcome does not advance the proposal's current owner revision exactly
    /// once.
    pub fn new(
        outcome_id: TransferOutcomeId,
        proposal: TransferProposal,
        new_owner_revision: OwnerRevision,
        suspension: ShareSuspensionIntent,
        approval_key_reference: ApprovalKeyReference,
    ) -> Result<Self, TransferOutcomeError> {
        if outcome_id != TransferOutcomeId::from_proposal_id(&proposal.proposal_id) {
            return Err(TransferOutcomeError::OutcomeId);
        }
        let expected = proposal
            .owner_revision
            .checked_next()
            .map_err(|_error| TransferOutcomeError::Revision)?;
        if new_owner_revision != expected {
            return Err(TransferOutcomeError::Revision);
        }
        Ok(Self {
            outcome_id,
            proposal,
            new_owner_revision,
            suspension,
            approval_algorithm: HostApprovalAlgorithm::Ed25519,
            approval_key_reference,
        })
    }

    /// Encodes the complete deterministic payload that the host approval key signs.
    ///
    /// The returned buffer owns nonce-bearing signing bytes and is zeroized on
    /// drop. This boundary cannot wipe copies made by a caller's signer or
    /// serializer implementation.
    ///
    /// # Errors
    ///
    /// Returns [`TransferOutcomeError::Revision`] when an impossible in-memory
    /// candidate violates its checked revision relation, or
    /// [`TransferOutcomeError::FieldLength`] if a field cannot fit its fixed
    /// binary length prefix.
    pub fn canonical_payload(&self) -> Result<Zeroizing<Vec<u8>>, TransferOutcomeError> {
        let expected = self
            .proposal
            .owner_revision
            .checked_next()
            .map_err(|_error| TransferOutcomeError::Revision)?;
        if self.new_owner_revision != expected {
            return Err(TransferOutcomeError::Revision);
        }
        let mut payload = Zeroizing::new(Vec::with_capacity(
            TRANSFER_OUTCOME_SIGNING_DOMAIN.len() + 512,
        ));
        payload.extend_from_slice(TRANSFER_OUTCOME_SIGNING_DOMAIN);
        append_signing_field(&mut payload, self.outcome_id.as_str())?;
        append_signing_field(&mut payload, self.proposal.relay_id.as_str())?;
        append_signing_field(&mut payload, self.proposal.host_id.as_str())?;
        payload.extend_from_slice(&self.proposal.owner_revision.get().to_be_bytes());
        payload.extend_from_slice(&self.new_owner_revision.get().to_be_bytes());
        append_signing_owner(&mut payload, &self.proposal.current_owner)?;
        append_signing_owner(&mut payload, &self.proposal.target)?;
        append_signing_field(&mut payload, self.proposal.proposal_id.as_str())?;
        append_signing_field(&mut payload, self.proposal.nonce.0.as_str())?;
        append_signing_field(&mut payload, &self.proposal.expiry.value)?;
        let suspension_tag = match self.suspension {
            ShareSuspensionIntent::AllActiveShares => ALL_ACTIVE_SHARES_TAG,
        };
        payload.push(suspension_tag);
        let algorithm_tag = match self.approval_algorithm {
            HostApprovalAlgorithm::Ed25519 => ED25519_APPROVAL_ALGORITHM_TAG,
        };
        payload.push(algorithm_tag);
        append_signing_field(&mut payload, self.approval_key_reference.as_str())?;
        Ok(payload)
    }

    /// Returns the complete proposal coordinates bound by this candidate.
    #[must_use]
    pub const fn proposal(&self) -> &TransferProposal {
        &self.proposal
    }

    /// Returns the committed owner revision.
    #[must_use]
    pub const fn new_owner_revision(&self) -> OwnerRevision {
        self.new_owner_revision
    }

    /// Returns the deterministic outcome identity.
    #[must_use]
    pub const fn outcome_id(&self) -> &TransferOutcomeId {
        &self.outcome_id
    }

    /// Returns the required share-suspension intent.
    #[must_use]
    pub const fn suspension(&self) -> ShareSuspensionIntent {
        self.suspension
    }

    /// Returns the fixed algorithm required to sign this outcome.
    #[must_use]
    pub const fn approval_algorithm(&self) -> HostApprovalAlgorithm {
        self.approval_algorithm
    }

    /// Returns the safe reference to the approval key selected for signing.
    #[must_use]
    pub const fn approval_key_reference(&self) -> &ApprovalKeyReference {
        &self.approval_key_reference
    }
}

impl Debug for TransferOutcomeCandidate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferOutcomeCandidate")
            .field("outcome_id", &self.outcome_id)
            .field("proposal", &self.proposal)
            .field("new_owner_revision", &self.new_owner_revision)
            .field("suspension", &self.suspension)
            .field("approval_algorithm", &self.approval_algorithm)
            .field("approval_key_reference", &self.approval_key_reference)
            .finish()
    }
}

impl<'de> Deserialize<'de> for TransferOutcomeCandidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireCandidate {
            outcome_id: TransferOutcomeId,
            proposal: TransferProposal,
            new_owner_revision: OwnerRevision,
            suspension: ShareSuspensionIntent,
            approval_key_reference: ApprovalKeyReference,
        }

        let wire = WireCandidate::deserialize(deserializer)?;
        Self::new(
            wire.outcome_id,
            wire.proposal,
            wire.new_owner_revision,
            wire.suspension,
            wire.approval_key_reference,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn append_signing_field(payload: &mut Vec<u8>, value: &str) -> Result<(), TransferOutcomeError> {
    let length = u32::try_from(value.len()).map_err(|_error| TransferOutcomeError::FieldLength)?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

fn append_signing_owner(
    payload: &mut Vec<u8>,
    owner: &HostOwner,
) -> Result<(), TransferOutcomeError> {
    match owner {
        HostOwner::Principal(id) => {
            payload.push(PRINCIPAL_OWNER_TAG);
            append_signing_field(payload, id.as_str())
        }
        HostOwner::Team(id) => {
            payload.push(TEAM_OWNER_TAG);
            append_signing_field(payload, id.as_str())
        }
    }
}

/// Durable host-signed result of one locally confirmed owner transfer.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct SignedTransferOutcome {
    candidate: TransferOutcomeCandidate,
    signature: HostApprovalSignature,
}

impl SignedTransferOutcome {
    /// Adds the host approval signature to a prepared outcome candidate.
    #[must_use]
    pub const fn new(
        candidate: TransferOutcomeCandidate,
        signature: HostApprovalSignature,
    ) -> Self {
        Self {
            candidate,
            signature,
        }
    }

    /// Returns the complete candidate bound by this host approval signature.
    #[must_use]
    pub const fn candidate(&self) -> &TransferOutcomeCandidate {
        &self.candidate
    }

    /// Returns the exact canonical payload that this signature must verify.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`TransferOutcomeCandidate::canonical_payload`].
    pub fn canonical_payload(&self) -> Result<Zeroizing<Vec<u8>>, TransferOutcomeError> {
        self.candidate.canonical_payload()
    }

    /// Returns the host approval signature for verifier input.
    #[must_use]
    pub const fn signature(&self) -> &HostApprovalSignature {
        &self.signature
    }
}

impl Debug for SignedTransferOutcome {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedTransferOutcome")
            .field("candidate", &self.candidate)
            .field("signature", &"[REDACTED]")
            .finish()
    }
}

impl<'de> Deserialize<'de> for SignedTransferOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireOutcome {
            candidate: TransferOutcomeCandidate,
            signature: HostApprovalSignature,
        }

        let wire = WireOutcome::deserialize(deserializer)?;
        Ok(Self::new(wire.candidate, wire.signature))
    }
}
