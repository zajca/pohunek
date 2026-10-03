import type {
  EnrollmentInfo,
  EnrollmentStatus,
  HostGovernanceStatus,
  HostOwner,
  QuarantineReason,
} from "@pohunek/protocol";
import { ClientError } from "./error";

const HOST_GOVERNANCE_FIELDS = [
  "host_id",
  "enrollment",
  "owner",
  "owner_revision",
  "quarantine",
  "approval_key_reference",
];
const ENROLLMENT_FIELDS = ["relay_id", "status", "revision"];
const OWNER_FIELDS = ["kind", "id"];
const GOVERNANCE_ID_PAYLOAD = "[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]";
const MAX_U64 = 18_446_744_073_709_551_615n;
const ENROLLMENT_STATUSES = new Set<EnrollmentStatus>([
  "disabled",
  "pending_local_commit",
  "active",
  "rotating",
  "quarantined",
  "locally_unenrolled",
]);
const QUARANTINE_REASONS = new Set<QuarantineReason>([
  "host_identity_clone",
  "projection_conflict",
  "enrollment_conflict",
]);

/** Decode the safe `host.governance.inspect` response contract. */
export function decodeHostGovernanceInspect(value: unknown): HostGovernanceStatus {
  if (!isRecordWithExactly(value, HOST_GOVERNANCE_FIELDS)) {
    throw invalidGovernanceInspectResponse();
  }

  const enrollment = decodeEnrollment(value["enrollment"]);
  const owner = decodeOwner(value["owner"]);
  const ownerRevision = value["owner_revision"];
  const quarantine = value["quarantine"];

  if (
    !isGovernanceId(value["host_id"], "host_")
    || !isGovernanceId(value["approval_key_reference"], "approval_key_")
    || !isNullableCanonicalPositiveRevision(ownerRevision)
    || !isNullableQuarantineReason(quarantine)
    || (enrollment === null) !== (owner === null)
    || (enrollment === null) !== (ownerRevision === null)
    || (enrollment?.status === "quarantined") !== (quarantine !== null)
  ) {
    throw invalidGovernanceInspectResponse();
  }

  return value as HostGovernanceStatus;
}

function decodeEnrollment(value: unknown): EnrollmentInfo | null {
  if (value === null) {
    return null;
  }
  if (!isRecordWithExactly(value, ENROLLMENT_FIELDS)) {
    throw invalidGovernanceInspectResponse();
  }
  if (
    !isGovernanceId(value["relay_id"], "relay_")
    || !isEnrollmentStatus(value["status"])
    || !isCanonicalPositiveRevision(value["revision"])
  ) {
    throw invalidGovernanceInspectResponse();
  }
  return value as EnrollmentInfo;
}

function decodeOwner(value: unknown): HostOwner | null {
  if (value === null) {
    return null;
  }
  if (!isRecordWithExactly(value, OWNER_FIELDS)) {
    throw invalidGovernanceInspectResponse();
  }
  if (value["kind"] === "principal" && isGovernanceId(value["id"], "principal_")) {
    return value as HostOwner;
  }
  if (value["kind"] === "team" && isGovernanceId(value["id"], "team_")) {
    return value as HostOwner;
  }
  throw invalidGovernanceInspectResponse();
}

function isRecordWithExactly(
  value: unknown,
  expectedFields: readonly string[],
): value is Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const fields = Object.keys(value);
  return fields.length === expectedFields.length && expectedFields.every((field) => fields.includes(field));
}

function isGovernanceId(value: unknown, prefix: string): value is string {
  return typeof value === "string" && new RegExp(`^${prefix}${GOVERNANCE_ID_PAYLOAD}$`).test(value);
}

function isCanonicalPositiveRevision(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]*$/.test(value) && BigInt(value) <= MAX_U64;
}

function isNullableCanonicalPositiveRevision(value: unknown): boolean {
  return value === null || isCanonicalPositiveRevision(value);
}

function isEnrollmentStatus(value: unknown): value is EnrollmentStatus {
  return typeof value === "string" && ENROLLMENT_STATUSES.has(value as EnrollmentStatus);
}

function isNullableQuarantineReason(value: unknown): value is QuarantineReason | null {
  return value === null || (typeof value === "string" && QUARANTINE_REASONS.has(value as QuarantineReason));
}

function invalidGovernanceInspectResponse(): ClientError {
  return ClientError.protocol({
    class: "daemon",
    code: "host_governance_inspect_contract_mismatch",
    msg: "invalid host.governance.inspect response",
  });
}
