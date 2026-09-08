//! Typed public contract for the optional Pohunek team relay.
//!
//! This crate contains serializable API types with redacted secret diagnostics.
//! Raw credentials exist only in explicit one-time delivery DTOs. These types
//! never construct authenticated actors or make authorization decisions.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-09-08

mod account;
mod auth;
mod error;
mod evidence;
mod id;
mod management;
mod revision;

pub use account::{
    AccountLinkRequest, AccountRecord, CreateServiceAccountRequest, CredentialKind,
    CredentialMutation, CredentialPage, CredentialRecord, IdentityRecord, PrincipalKind,
    PrincipalState, RevokeCredentialRequest, RotateCredentialRequest, ServiceAccountCreated,
    ServiceAccountPage, ServiceAccountRecord,
};
pub use auth::{DeviceCredential, DeviceLoginStart, DevicePollResult, LoginId, Secret};
pub use error::{ApiError, ApiErrorBody};
pub use evidence::{
    EvidenceClaims, EvidenceMethod, EvidenceOutcome, EvidenceProvider, EvidenceVersion,
    SignedEvidence, EVIDENCE_SCHEMA_VERSION,
};
pub use id::{CredentialId, PrincipalId, RelayId, TeamId};
pub use management::{
    CreateGrantRequest, CreateGroupRequest, CreateRoleRequest, CreateTeamRequest, GrantSubject,
    Idempotency, MemberChangeRequest, MutationReceipt, Page, PageRequest, Record, RemoveRequest,
    RoleAssignmentRequest, UpdateGrantRequest, UpdateGroupRequest, UpdateRoleRequest,
    UpdateTeamRequest,
};

/// Current relay HTTP API version.
pub const API_VERSION: u16 = 1;
