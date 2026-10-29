//! Guard against governance and identity record changes that skip a format bump.
//!
//! The recursive key set and the serialized enum value sets of fully populated
//! `governance.json` and `identity.json` records are compared with the checked-in
//! snapshots `fixtures/governance-<N>.txt` and `fixtures/identity-<N>.txt` for the
//! current [`GOVERNANCE_FORMAT_VERSION`] and [`IDENTITY_FORMAT_VERSION`]. Each
//! reader accepts exactly one format, so a daemon that meets a record with a new
//! key or enum value refuses to start. Every enum is listed through
//! `unit_values!` or `data_values!` and every string leaf must
//! be a registered enum path or free text, as in `store::shape_guard`.
//!
//! The top-level wire structs are destructured field by field and each enum-valued
//! field is pinned to its type. The protocol types nested below them keep private
//! fields, so their keys and values are covered by the populated samples only.
//!
//! When a snapshot no longer matches, the test fails. To resolve it:
//!
//! 1. bump the format constant,
//! 2. make the reader accept the previous format as well,
//! 3. add the new `fixtures/<record>-<new>.txt` with the content the failure prints
//!    and register it in the snapshot table.

use std::collections::BTreeSet;

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use protocol::{EnrollmentStatus, HostOwner, QuarantineReason, ShareSuspensionIntent};
use serde_json::Value;

use super::{
    GovernanceState, GovernanceWireOwned, IdentityBootstrapPhase, IdentityRecord,
    IdentityWireOwned, RetiredEnrollment, GOVERNANCE_FORMAT_VERSION, IDENTITY_FORMAT_VERSION,
};
use crate::host_state::approval_key::{generate_host_id, ApprovalKeyRecord};
use crate::store::shape_guard::{
    classify_leaves, collect_paths, collect_string_leaves, data_values, string_values, unit_values,
    EnumEntry, Leaves,
};

/// Governance key and value snapshots by format version.
const GOVERNANCE_SNAPSHOTS: &[(u8, &str)] = &[(2, include_str!("../fixtures/governance-2.txt"))];

/// Identity key and value snapshots by format version.
const IDENTITY_SNAPSHOTS: &[(u8, &str)] = &[(2, include_str!("../fixtures/identity-2.txt"))];

fn identifier(prefix: &str, byte: u8) -> String {
    format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode([byte; 32]))
}

/// Governance states that together populate every optional section: a current
/// enrollment with a signed outcome, a quarantine with its origin, and a retired
/// enrollment that carries its own signed outcome.
fn governance_samples() -> Vec<GovernanceState> {
    let identity = generate_host_id().expect("host identity");
    let key = ApprovalKeyRecord::generate(&identity).expect("approval key");
    let relay = protocol::RelayId::parse(&identifier("relay_", 1)).expect("relay");
    let principal = HostOwner::Principal(
        protocol::PrincipalId::parse(&identifier("principal_", 2)).expect("principal"),
    );
    let team = HostOwner::Team(protocol::TeamId::parse(&identifier("team_", 3)).expect("team"));
    let proposal = protocol::TransferProposal::new(
        protocol::TransferCoordinates::new(
            relay.clone(),
            identity.clone(),
            protocol::OwnerRevision::new(1).expect("proposal revision"),
            principal.clone(),
            team.clone(),
        ),
        protocol::ProposalId::parse(&identifier("proposal_", 4)).expect("proposal"),
        protocol::ProposalNonce::parse(&identifier("nonce_", 5)).expect("nonce"),
        protocol::ProposalExpiry::parse("2026-09-03T10:00:00Z").expect("expiry"),
    );
    let candidate = protocol::TransferOutcomeCandidate::new(
        protocol::TransferOutcomeId::from_proposal_id(proposal.proposal_id()),
        proposal,
        protocol::OwnerRevision::new(2).expect("outcome revision"),
        ShareSuspensionIntent::AllActiveShares,
        key.key.reference().clone(),
    )
    .expect("advance revision");
    let payload = candidate.canonical_payload().expect("canonical payload");
    let outcome = protocol::SignedTransferOutcome::new(
        candidate,
        key.key.sign(&payload).expect("sign outcome"),
    );
    let enrollment = |status, revision| {
        protocol::EnrollmentInfo::new(
            relay.clone(),
            status,
            protocol::EnrollmentRevision::new(revision).expect("enrollment revision"),
        )
    };
    let status = |enrollment, owner, owner_revision, quarantine| {
        protocol::HostGovernanceStatus::new(
            identity.clone(),
            Some(enrollment),
            Some(owner),
            Some(protocol::OwnerRevision::new(owner_revision).expect("owner revision")),
            quarantine,
            key.key.reference().clone(),
        )
        .expect("valid governance status")
    };

    let current = GovernanceState::new(
        status(
            enrollment(EnrollmentStatus::Active, 1),
            team.clone(),
            2,
            None,
        ),
        Some(outcome.clone()),
        None,
    )
    .expect("current governance");
    let quarantined = GovernanceState::with_quarantine_origin(
        status(
            enrollment(EnrollmentStatus::Quarantined, 2),
            principal,
            1,
            Some(QuarantineReason::ProjectionConflict),
        ),
        Some(EnrollmentStatus::Active),
        None,
        None,
    )
    .expect("quarantined governance");
    let retired = RetiredEnrollment::new(
        enrollment(EnrollmentStatus::LocallyUnenrolled, 1),
        team.clone(),
        protocol::OwnerRevision::new(2).expect("retired owner revision"),
        Some(outcome),
    )
    .expect("retired summary");
    let following = GovernanceState::new(
        status(
            enrollment(EnrollmentStatus::PendingLocalCommit, 2),
            team,
            3,
            None,
        ),
        None,
        Some(retired),
    )
    .expect("governance following a retired enrollment");
    vec![current, quarantined, following]
}

/// Fails to compile when a wire field is added or an enum field is retyped.
#[expect(
    clippy::unneeded_field_pattern,
    reason = "naming every field with `_` is what makes a field change a compile error"
)]
fn assert_wire_fields_are_reviewed(governance: &GovernanceState, identity: &IdentityRecord) {
    let wire: GovernanceWireOwned =
        serde_json::from_slice(&governance.encode().expect("encode")).expect("governance wire");
    let GovernanceWireOwned {
        format_version: _,
        host_id: _,
        approval_key_reference: _,
        enrollment: _,
        owner,
        owner_revision: _,
        quarantine,
        quarantine_origin,
        latest_transfer_outcome: _,
        retired_enrollment: _,
    } = wire;
    let _: &Option<HostOwner> = &owner;
    let _: &Option<QuarantineReason> = &quarantine;
    let _: &Option<EnrollmentStatus> = &quarantine_origin;

    let wire: IdentityWireOwned =
        serde_json::from_slice(&identity.encode().expect("encode")).expect("identity wire");
    let IdentityWireOwned {
        format_version: _,
        host_id: _,
        phase,
    } = wire;
    let _: &IdentityBootstrapPhase = &phase;
}

fn host_owner_values() -> Vec<String> {
    let principal = || {
        HostOwner::Principal(
            protocol::PrincipalId::parse(&identifier("principal_", 2)).expect("principal"),
        )
    };
    let team = || HostOwner::Team(protocol::TeamId::parse(&identifier("team_", 3)).expect("team"));
    data_values!(
        HostOwner, Some("kind");
        HostOwner::Principal(_) => principal(),
        HostOwner::Team(_) => team(),
    )
}

const OWNER_KIND_PATHS: &[&str] = &[
    "governance.owner.kind",
    "governance.retired_enrollment.owner.kind",
    "governance.latest_transfer_outcome.candidate.proposal.current_owner.kind",
    "governance.latest_transfer_outcome.candidate.proposal.target.kind",
    "governance.retired_enrollment.latest_transfer_outcome.candidate.proposal.current_owner.kind",
    "governance.retired_enrollment.latest_transfer_outcome.candidate.proposal.target.kind",
];

fn governance_enums() -> Vec<EnumEntry> {
    let paths = |list: &[&str]| list.iter().map(|path| (*path).to_owned()).collect();
    vec![
        EnumEntry {
            paths: paths(&[
                "governance.enrollment.status",
                "governance.quarantine_origin",
                "governance.retired_enrollment.enrollment.status",
            ]),
            values: unit_values!(
                EnrollmentStatus: Disabled, PendingLocalCommit, Active, Rotating, Quarantined,
                LocallyUnenrolled
            ),
        },
        EnumEntry {
            paths: paths(&["governance.quarantine"]),
            values: unit_values!(
                QuarantineReason: HostIdentityClone, ProjectionConflict, EnrollmentConflict; skip Unknown
            ),
        },
        EnumEntry {
            paths: paths(OWNER_KIND_PATHS),
            values: host_owner_values(),
        },
        EnumEntry {
            paths: paths(&[
                "governance.latest_transfer_outcome.candidate.suspension",
                "governance.retired_enrollment.latest_transfer_outcome.candidate.suspension",
            ]),
            values: unit_values!(ShareSuspensionIntent: AllActiveShares),
        },
    ]
}

fn identity_enums() -> Vec<EnumEntry> {
    vec![EnumEntry {
        paths: vec!["identity.phase".to_owned()],
        values: unit_values!(IdentityBootstrapPhase: InProgress, Complete),
    }]
}

/// Identifiers, signatures, key references, timestamps and decimal revision counters.
fn governance_free_text() -> Vec<Vec<String>> {
    let mut paths = vec![
        "governance.host_id".to_owned(),
        "governance.approval_key_reference".to_owned(),
        "governance.enrollment.relay_id".to_owned(),
        "governance.owner.id".to_owned(),
        "governance.owner_revision".to_owned(),
        "governance.enrollment.revision".to_owned(),
        "governance.retired_enrollment.owner_revision".to_owned(),
        "governance.retired_enrollment.enrollment.revision".to_owned(),
        "governance.retired_enrollment.enrollment.relay_id".to_owned(),
        "governance.retired_enrollment.owner.id".to_owned(),
    ];
    for outcome in [
        "governance.latest_transfer_outcome",
        "governance.retired_enrollment.latest_transfer_outcome",
    ] {
        for relative in [
            "signature",
            "candidate.outcome_id",
            "candidate.approval_key_reference",
            "candidate.proposal.relay_id",
            "candidate.proposal.host_id",
            "candidate.proposal.current_owner.id",
            "candidate.proposal.target.id",
            "candidate.proposal.proposal_id",
            "candidate.proposal.nonce",
            "candidate.proposal.expiry",
            "candidate.proposal.owner_revision",
            "candidate.new_owner_revision",
        ] {
            paths.push(format!("{outcome}.{relative}"));
        }
    }
    paths.into_iter().map(|path| vec![path]).collect()
}

fn identity_free_text() -> Vec<Vec<String>> {
    vec![vec!["identity.host_id".to_owned()]]
}

/// Key and `path=value` lines of `samples` serialized under `kind`.
fn shape_of(
    kind: &str,
    documents: &[Value],
    enums: &[EnumEntry],
    free: &[Vec<String>],
) -> BTreeSet<String> {
    let mut shape = BTreeSet::from([kind.to_owned()]);
    let mut leaves = Leaves::new();
    for document in documents {
        let mut paths = BTreeSet::new();
        collect_paths("", document, &mut paths);
        shape.extend(paths.into_iter().map(|path| format!("{kind}.{path}")));
        let mut found = Leaves::new();
        collect_string_leaves("", document, &mut found);
        for (path, values) in found {
            leaves
                .entry(format!("{kind}.{path}"))
                .or_default()
                .extend(values);
        }
    }
    shape.extend(
        classify_leaves(&leaves, enums, free).unwrap_or_else(|message| panic!("{message}")),
    );
    shape
}

fn governance_shape() -> BTreeSet<String> {
    let samples = governance_samples();
    assert_wire_fields_are_reviewed(
        &samples[0],
        &IdentityRecord::complete(generate_host_id().expect("host identity")),
    );
    let documents: Vec<Value> = samples
        .iter()
        .map(|state| serde_json::from_slice(&state.encode().expect("encode")).expect("json"))
        .collect();
    shape_of(
        "governance",
        &documents,
        &governance_enums(),
        &governance_free_text(),
    )
}

fn identity_shape() -> BTreeSet<String> {
    let identity = generate_host_id().expect("host identity");
    let documents: Vec<Value> = [
        IdentityRecord::in_progress(identity.clone()),
        IdentityRecord::complete(identity),
    ]
    .iter()
    .map(|record| serde_json::from_slice(&record.encode().expect("encode")).expect("json"))
    .collect();
    shape_of(
        "identity",
        &documents,
        &identity_enums(),
        &identity_free_text(),
    )
}

/// Compares `actual` with the snapshot kept for `version`.
fn check_format(
    record: &str,
    constant: &str,
    version: u8,
    actual: &BTreeSet<String>,
    snapshots: &[(u8, &str)],
) -> Result<(), String> {
    let rendered: String = actual
        .iter()
        .flat_map(|line| [line.as_str(), "\n"])
        .collect();
    let Some((_, expected)) = snapshots.iter().find(|(known, _)| *known == version) else {
        return Err(format!(
            "{record} format {version} has no snapshot. Add \
             crates/daemon/src/host_state/fixtures/{record}-{version}.txt with the content \
             below and register it:\n{rendered}"
        ));
    };
    let expected: BTreeSet<String> = expected.lines().map(str::to_owned).collect();
    if &expected == actual {
        return Ok(());
    }
    let added: Vec<_> = actual.difference(&expected).collect();
    let removed: Vec<_> = expected.difference(actual).collect();
    Err(format!(
        "the serialized {record} record changed without a format bump ({constant} = {version}).\n  \
         added: {added:?}\n  removed: {removed:?}\n\
         Bump {constant}, make the reader accept the previous format as well, and add \
         fixtures/{record}-<new>.txt with the content below:\n{rendered}"
    ))
}

#[test]
fn the_governance_record_matches_the_snapshot_of_the_current_format() {
    if let Err(message) = check_format(
        "governance",
        "GOVERNANCE_FORMAT_VERSION",
        GOVERNANCE_FORMAT_VERSION,
        &governance_shape(),
        GOVERNANCE_SNAPSHOTS,
    ) {
        panic!("{message}");
    }
}

#[test]
fn the_identity_record_matches_the_snapshot_of_the_current_format() {
    if let Err(message) = check_format(
        "identity",
        "IDENTITY_FORMAT_VERSION",
        IDENTITY_FORMAT_VERSION,
        &identity_shape(),
        IDENTITY_SNAPSHOTS,
    ) {
        panic!("{message}");
    }
}

#[test]
fn a_changed_governance_value_set_is_reported_as_an_unformatted_change() {
    let mut shape = governance_shape();
    shape.insert("governance.enrollment.status=suspended".to_owned());
    let message = check_format(
        "governance",
        "GOVERNANCE_FORMAT_VERSION",
        GOVERNANCE_FORMAT_VERSION,
        &shape,
        GOVERNANCE_SNAPSHOTS,
    )
    .expect_err("an added enum value must fail the guard");
    assert!(message.contains("without a format bump"), "{message}");
    assert!(message.contains("accept the previous format"), "{message}");
    assert!(
        message.contains("governance.enrollment.status=suspended"),
        "{message}"
    );

    let mut shape = governance_shape();
    shape.insert("governance.new_key".to_owned());
    check_format(
        "governance",
        "GOVERNANCE_FORMAT_VERSION",
        GOVERNANCE_FORMAT_VERSION,
        &shape,
        GOVERNANCE_SNAPSHOTS,
    )
    .expect_err("an added key must fail the guard");

    let mut shape = identity_shape();
    shape.insert("identity.phase=retired".to_owned());
    let message = check_format(
        "identity",
        "IDENTITY_FORMAT_VERSION",
        IDENTITY_FORMAT_VERSION,
        &shape,
        IDENTITY_SNAPSHOTS,
    )
    .expect_err("an added phase must fail the guard");
    assert!(message.contains("identity.phase=retired"), "{message}");
}

#[test]
fn a_bumped_format_without_a_snapshot_is_reported() {
    let message = check_format(
        "governance",
        "GOVERNANCE_FORMAT_VERSION",
        GOVERNANCE_FORMAT_VERSION + 1,
        &governance_shape(),
        GOVERNANCE_SNAPSHOTS,
    )
    .expect_err("a new format needs its own snapshot");
    assert!(message.contains("has no snapshot"), "{message}");
}

#[test]
fn a_bumped_identity_format_without_a_snapshot_is_reported() {
    let message = check_format(
        "identity",
        "IDENTITY_FORMAT_VERSION",
        IDENTITY_FORMAT_VERSION + 1,
        &identity_shape(),
        IDENTITY_SNAPSHOTS,
    )
    .expect_err("a new identity format needs its own snapshot");
    assert!(message.contains("has no snapshot"), "{message}");
}
