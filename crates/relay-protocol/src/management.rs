//! Stable management request and response contracts.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Required correlation and retry coordinates for a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct Idempotency {
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}

/// Bounded UUID cursor page request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    pub after: Option<Uuid>,
    pub limit: u16,
}

/// A stable page of revisioned records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub records: Vec<Record>,
}

/// Public UUID coordinate and optimistic revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: Uuid,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub revision: i64,
}

/// A current team membership coordinate and optimistic revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct MemberRecord {
    pub principal_id: Uuid,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub revision: i64,
}

/// A bounded cursor page of current team memberships.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct MembershipPage {
    pub records: Vec<MemberRecord>,
    pub next_cursor: Option<Uuid>,
}

/// Response after an idempotent mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct MutationReceipt {
    pub record: Option<Record>,
}

/// Creates a team with an explicitly selected initial owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CreateTeamRequest {
    pub initial_owner: Uuid,
    pub display_name: String,
    pub idempotency: Idempotency,
}
/// Updates a team with an exact optimistic revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct UpdateTeamRequest {
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub display_name: Option<String>,
    pub disable: bool,
    pub idempotency: Idempotency,
}
/// Changes a member role or removes the member when role is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct MemberChangeRequest {
    pub principal_id: Uuid,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub role: Option<String>,
    pub idempotency: Idempotency,
}
/// Creates a team group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CreateGroupRequest {
    pub display_name: String,
    pub idempotency: Idempotency,
}
/// Updates a team group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct UpdateGroupRequest {
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub display_name: String,
    pub idempotency: Idempotency,
}
/// Removes a revisioned resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct RemoveRequest {
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub idempotency: Idempotency,
}
/// Changes a group member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct RoleAssignmentRequest {
    pub principal_id: Uuid,
    pub assign: bool,
    pub idempotency: Idempotency,
}
/// Creates a custom role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CreateRoleRequest {
    pub display_name: String,
    pub permissions: Vec<String>,
    pub idempotency: Idempotency,
}
/// Updates a custom role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct UpdateRoleRequest {
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub display_name: String,
    pub permissions: Vec<String>,
    pub idempotency: Idempotency,
}
/// Grant subject constrained to the enclosing team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum GrantSubject {
    Principal(Uuid),
    ServiceAccount(Uuid),
    Group(Uuid),
}
/// Creates a narrowed stable-permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CreateGrantRequest {
    pub subject: GrantSubject,
    pub resource_kind: String,
    pub resource_id: String,
    pub permission: String,
    pub idempotency: Idempotency,
}
/// Updates a narrowed stable-permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct UpdateGrantRequest {
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub subject: GrantSubject,
    pub resource_kind: String,
    pub resource_id: String,
    pub permission: String,
    pub idempotency: Idempotency,
}

#[cfg(test)]
mod tests {
    use super::{GrantSubject, MemberRecord, MembershipPage};
    use uuid::Uuid;

    #[test]
    fn grant_subject_rejects_unknown_shape() {
        serde_json::from_str::<GrantSubject>(
            r#"{"kind":"group","id":"00000000-0000-0000-0000-000000000000","extra":true}"#,
        )
        .expect_err("unknown grant fields must be rejected");
    }

    #[test]
    fn membership_page_serializes_revisions_as_decimal_strings() {
        let page = MembershipPage {
            records: vec![MemberRecord {
                principal_id: Uuid::nil(),
                revision: 7,
            }],
            next_cursor: None,
        };
        let value = serde_json::to_value(&page).expect("serialize membership page");
        assert_eq!(value["records"][0]["revision"], "7");
        assert_eq!(
            serde_json::from_value::<MembershipPage>(value).expect("deserialize membership page"),
            page
        );
    }
}
