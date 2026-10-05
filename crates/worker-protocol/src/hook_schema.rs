//! Core-owned hook schemas.
//!
//! A hook schema is a finite, compiled-in description of what a runtime's
//! lifecycle hooks may report to the PTY-owning worker and what the daemon
//! accepts when it re-imports that state. A runtime descriptor names a schema
//! by id and never carries its contents; the registry below is the closed set
//! of ids. The schema only selects which core validators apply and with which
//! finite enumeration: the ancestry matcher, the subagent sequence rule, and
//! the optional subagent fields are declared by the schema and applied by the
//! worker and the daemon through the types below. Expiry and self-target
//! checks stay in the worker and the daemon and are never relaxed by a schema.

// Rust guideline compliant 2026-10-05

use std::fmt::{Display, Formatter};

/// Hook operation a schema may admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookAction {
    /// Reports the active provider identity and its native reference.
    IdentityReport,
    /// Releases the active provider identity.
    IdentityRelease,
    /// Records the start of a provider-managed subagent.
    SubagentStart,
    /// Records the end of a provider-managed subagent.
    SubagentStop,
    /// Forwards a provider notification.
    Notification,
}

impl HookAction {
    /// Stable lowercase name used in logs and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityReport => "identity_report",
            Self::IdentityRelease => "identity_release",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::Notification => "notification_create",
        }
    }
}

impl Display for HookAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Shape of a provider-native session reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// An opaque session identifier.
    Id,
    /// A transcript or session file path.
    Path,
}

impl ReferenceKind {
    /// Wire spelling of the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Path => "path",
        }
    }

    /// Parses the wire spelling of a kind.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "id" => Some(Self::Id),
            "path" => Some(Self::Path),
            _ => None,
        }
    }
}

/// How a reporting process must relate to the managed PTY tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestryMatcher {
    /// The reported process is the PTY's root process or one of its
    /// descendants.
    ManagedPtyDescendant,
}

/// Where a reported process sits relative to the managed PTY tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyRelation {
    /// The process is the PTY's root process or one of its descendants.
    InTree,
    /// The process is not part of the PTY's tree.
    Outside,
}

impl PtyRelation {
    /// The relation of a process that is, or is not, in the PTY's tree.
    #[must_use]
    pub const fn from_in_tree(in_tree: bool) -> Self {
        if in_tree {
            Self::InTree
        } else {
            Self::Outside
        }
    }
}

impl AncestryMatcher {
    /// The matcher every claim is held to when no schema names one, such as a
    /// launch identity reported by a session without a hook schema.
    pub const CORE: Self = Self::ManagedPtyDescendant;

    /// Whether a process in `relation` to the PTY satisfies the matcher.
    #[must_use]
    pub const fn admits(self, relation: PtyRelation) -> bool {
        match self {
            Self::ManagedPtyDescendant => matches!(relation, PtyRelation::InTree),
        }
    }
}

/// Ordering a schema requires between the claims of one subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentSequence {
    /// A start is admitted once per subagent. A stop is admitted when no start
    /// was recorded or when its sequence is greater than the recorded one.
    StopAfterStart,
}

impl SubagentSequence {
    /// Whether a stop claim carrying `reported` is ordered after the claim
    /// recorded for the same subagent, if any.
    #[must_use]
    pub const fn admits_stop(self, recorded: Option<u64>, reported: u64) -> bool {
        match self {
            Self::StopAfterStart => match recorded {
                Some(recorded) => reported > recorded,
                None => true,
            },
        }
    }
}

/// Optional field of a subagent claim that a schema may admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubagentField {
    /// The identifier of the subagent's parent.
    ParentId,
    /// The provider-defined subagent type.
    AgentType,
    /// The terminal outcome carried by a stop claim.
    Outcome,
}

/// Why a schema refuses a hook operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDenial {
    /// The session has no schema, or the schema does not admit the action.
    ActionNotAllowed,
    /// The provider is outside the schema's provider set for the action.
    ProviderNotAllowed,
}

/// Checks one hook operation against a session's schema.
///
/// This is the single admission rule the worker applies to its socket and the
/// daemon applies to the public socket, so a refusal on one path cannot be
/// bypassed through the other. A session without a schema admits nothing.
///
/// # Errors
///
/// Returns [`HookDenial::ActionNotAllowed`] when `schema` is absent or does
/// not admit `action`, and [`HookDenial::ProviderNotAllowed`] when `provider`
/// may not report that action for a session launched as `launch_runtime`.
pub fn admit_hook(
    schema: Option<&'static HookSchema>,
    action: HookAction,
    provider: &str,
    launch_runtime: Option<&str>,
) -> Result<&'static HookSchema, HookDenial> {
    let schema = schema
        .filter(|schema| schema.allows(action))
        .ok_or(HookDenial::ActionNotAllowed)?;
    let admitted = match action {
        HookAction::SubagentStart | HookAction::SubagentStop => {
            schema.admits_subagent_provider(provider)
        }
        HookAction::IdentityReport | HookAction::IdentityRelease | HookAction::Notification => {
            schema.admits_identity_provider(provider, launch_runtime)
        }
    };
    if admitted {
        Ok(schema)
    } else {
        Err(HookDenial::ProviderNotAllowed)
    }
}

/// One compiled hook schema.
#[derive(Debug, PartialEq, Eq)]
pub struct HookSchema {
    /// Registry id a runtime descriptor names.
    pub id: &'static str,
    /// Integration handler ids that drive this schema.
    pub handlers: &'static [&'static str],
    /// Provider ids other than the session's own runtime that may report
    /// identity and notifications while hosted in the foreground.
    pub identity_providers: &'static [&'static str],
    /// Provider ids allowed to report subagent lifecycle.
    pub subagent_providers: &'static [&'static str],
    /// Hook operations the schema admits.
    pub actions: &'static [HookAction],
    /// Native-reference kinds a report may carry.
    pub reference_kinds: &'static [ReferenceKind],
    /// Ancestry rule for the reported process.
    pub ancestry: AncestryMatcher,
    /// Whether a provider other than the session's launch runtime may become
    /// the active identity.
    pub nested_active: bool,
    /// Optional subagent claim fields the schema admits.
    pub subagent_fields: &'static [SubagentField],
    /// Ordering rule of subagent claims. A schema that admits subagent
    /// operations declares one; a schema without one admits no subagent record.
    pub subagent_sequence: Option<SubagentSequence>,
}

impl HookSchema {
    /// Whether the schema admits `action`.
    #[must_use]
    pub fn allows(&self, action: HookAction) -> bool {
        self.actions.contains(&action)
    }

    /// Whether `provider` may report identity for a session launched as
    /// `launch_runtime`.
    ///
    /// The launch runtime itself is always the session's own provider. Any
    /// other provider is a nested active identity, which the schema must
    /// enable and list.
    #[must_use]
    pub fn admits_identity_provider(&self, provider: &str, launch_runtime: Option<&str>) -> bool {
        launch_runtime == Some(provider)
            || (self.nested_active && self.identity_providers.contains(&provider))
    }

    /// Whether `provider` may report subagent lifecycle.
    #[must_use]
    pub fn admits_subagent_provider(&self, provider: &str) -> bool {
        self.subagent_providers.contains(&provider)
    }

    /// Whether the schema admits a native reference of the wire `kind`.
    #[must_use]
    pub fn admits_reference_kind(&self, kind: &str) -> bool {
        ReferenceKind::parse(kind).is_some_and(|kind| self.reference_kinds.contains(&kind))
    }

    /// Whether the schema admits subagent records at all: it admits a subagent
    /// provider and declares how their claims are ordered.
    #[must_use]
    pub fn admits_subagents(&self) -> bool {
        self.subagent_sequence.is_some() && !self.subagent_providers.is_empty()
    }

    /// Whether the schema admits the optional subagent `field`.
    #[must_use]
    pub fn admits_subagent_field(&self, field: SubagentField) -> bool {
        self.subagent_fields.contains(&field)
    }

    /// Whether the integration handler `handler` drives this schema.
    #[must_use]
    pub fn supports_handler(&self, handler: &str) -> bool {
        self.handlers.contains(&handler)
    }
}

/// Provider ids whose hooks may report identity inside any session that has
/// an identity schema: the session's own runtime and the agents a session can
/// host in the foreground.
const HOOKED_PROVIDERS: &[&str] = &["shell", "codex", "claude", "hermes"];

/// Provider ids whose hooks report provider-managed subagents.
const SUBAGENT_PROVIDERS: &[&str] = &["codex", "claude"];

/// Hook operations of a runtime that reports identity and notifications only.
const IDENTITY_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::IdentityRelease,
    HookAction::Notification,
];

/// Hook operations of a runtime that also reports subagent lifecycle.
const IDENTITY_SUBAGENT_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::IdentityRelease,
    HookAction::SubagentStart,
    HookAction::SubagentStop,
    HookAction::Notification,
];

/// Native-reference kinds every shipped runtime reports.
const REFERENCE_KINDS: &[ReferenceKind] = &[ReferenceKind::Id, ReferenceKind::Path];

/// Identity, release, and notification reports without subagents.
const IDENTITY_V1: HookSchema = HookSchema {
    id: "identity-v1",
    handlers: &["hermes-hook-v1"],
    identity_providers: HOOKED_PROVIDERS,
    subagent_providers: &[],
    actions: IDENTITY_ACTIONS,
    reference_kinds: REFERENCE_KINDS,
    ancestry: AncestryMatcher::ManagedPtyDescendant,
    nested_active: true,
    subagent_fields: &[],
    subagent_sequence: None,
};

/// Identity, release, notification, and subagent lifecycle reports.
const IDENTITY_SUBAGENT_V1: HookSchema = HookSchema {
    id: "identity-subagent-v1",
    handlers: &["codex-hook-v1", "claude-hook-v1"],
    identity_providers: HOOKED_PROVIDERS,
    subagent_providers: SUBAGENT_PROVIDERS,
    actions: IDENTITY_SUBAGENT_ACTIONS,
    reference_kinds: REFERENCE_KINDS,
    ancestry: AncestryMatcher::ManagedPtyDescendant,
    nested_active: true,
    subagent_fields: &[
        SubagentField::ParentId,
        SubagentField::AgentType,
        SubagentField::Outcome,
    ],
    subagent_sequence: Some(SubagentSequence::StopAfterStart),
};

/// The closed set of hook schemas, in a stable order.
static REGISTRY: [&HookSchema; 2] = [&IDENTITY_V1, &IDENTITY_SUBAGENT_V1];

/// Resolves a schema id against the closed registry.
#[must_use]
pub fn hook_schema(id: &str) -> Option<&'static HookSchema> {
    REGISTRY.iter().copied().find(|schema| schema.id == id)
}

/// Every registered schema, in a stable order.
#[must_use]
pub fn hook_schemas() -> &'static [&'static HookSchema] {
    &REGISTRY
}

/// The schema an integration handler drives, when exactly one registered
/// schema does.
///
/// A handler that drives several schemas (or none) has no unambiguous schema,
/// so the answer is `None`.
#[must_use]
pub fn hook_schema_for_handler(handler: &str) -> Option<&'static HookSchema> {
    let mut driven = REGISTRY
        .iter()
        .copied()
        .filter(|schema| schema.supports_handler(handler));
    let first = driven.next()?;
    driven.next().is_none().then_some(first)
}

/// Whether any registered schema is driven by the integration handler
/// `handler`.
#[must_use]
pub fn is_known_hook_handler(handler: &str) -> bool {
    REGISTRY
        .iter()
        .any(|schema| schema.supports_handler(handler))
}

#[cfg(test)]
mod tests {
    use super::{
        hook_schema, hook_schemas, is_known_hook_handler, AncestryMatcher, HookAction, PtyRelation,
        ReferenceKind, SubagentField, SubagentSequence,
    };

    #[test]
    fn registry_is_a_closed_set_with_unique_ids() {
        let ids: Vec<&str> = hook_schemas().iter().map(|schema| schema.id).collect();
        assert_eq!(ids, ["identity-v1", "identity-subagent-v1"]);
        for id in &ids {
            assert_eq!(hook_schema(id).map(|schema| schema.id), Some(*id));
        }
        for unknown in ["", "identity", "Identity-V1", "identity-v1 ", "identity-v2"] {
            assert!(
                hook_schema(unknown).is_none(),
                "{unknown:?} must be refused"
            );
        }
    }

    #[test]
    fn every_handler_belongs_to_exactly_one_schema() {
        for handler in ["codex-hook-v1", "claude-hook-v1", "hermes-hook-v1"] {
            assert!(is_known_hook_handler(handler), "{handler}");
            let owners = hook_schemas()
                .iter()
                .filter(|schema| schema.supports_handler(handler))
                .count();
            assert_eq!(owners, 1, "{handler}");
        }
        assert!(!is_known_hook_handler("acme-v1"));
        assert!(!is_known_hook_handler(""));
    }

    #[test]
    fn a_handler_resolves_to_a_schema_only_when_exactly_one_is_driven() {
        assert_eq!(
            super::hook_schema_for_handler("hermes-hook-v1").map(|schema| schema.id),
            Some("identity-v1")
        );
        assert_eq!(
            super::hook_schema_for_handler("codex-hook-v1").map(|schema| schema.id),
            Some("identity-subagent-v1")
        );
        assert!(super::hook_schema_for_handler("acme-v1").is_none());
    }

    #[test]
    fn identity_schema_has_no_subagent_surface() {
        let schema = hook_schema("identity-v1").expect("registered");
        assert!(schema.allows(HookAction::IdentityReport));
        assert!(schema.allows(HookAction::IdentityRelease));
        assert!(schema.allows(HookAction::Notification));
        assert!(!schema.allows(HookAction::SubagentStart));
        assert!(!schema.allows(HookAction::SubagentStop));
        assert!(!schema.admits_subagent_provider("codex"));
        assert!(!schema.admits_subagent_field(SubagentField::ParentId));
    }

    #[test]
    fn subagent_schema_admits_only_the_subagent_providers() {
        let schema = hook_schema("identity-subagent-v1").expect("registered");
        for provider in ["codex", "claude"] {
            assert!(schema.admits_subagent_provider(provider), "{provider}");
        }
        for provider in ["", "shell", "hermes", "Codex", "codex "] {
            assert!(!schema.admits_subagent_provider(provider), "{provider:?}");
        }
        assert!(schema.allows(HookAction::SubagentStart));
        assert!(schema.allows(HookAction::SubagentStop));
        assert!(schema.admits_subagent_field(SubagentField::Outcome));
    }

    #[test]
    fn identity_provider_set_is_exact() {
        for schema in hook_schemas() {
            for provider in ["shell", "codex", "claude", "hermes"] {
                assert!(
                    schema.admits_identity_provider(provider, Some("codex")),
                    "{} must admit {provider}",
                    schema.id
                );
            }
            for provider in ["", "Hermes", "HERMES", "hermes ", "unknown"] {
                assert!(
                    !schema.admits_identity_provider(provider, Some("codex")),
                    "{} must refuse {provider:?}",
                    schema.id
                );
            }
        }
    }

    #[test]
    fn reference_kinds_parse_exactly() {
        let schema = hook_schema("identity-v1").expect("registered");
        assert!(schema.admits_reference_kind("id"));
        assert!(schema.admits_reference_kind("path"));
        for kind in ["", "ID", "Path", "uri", "id "] {
            assert!(!schema.admits_reference_kind(kind), "{kind:?}");
        }
        assert_eq!(ReferenceKind::parse("id"), Some(ReferenceKind::Id));
        assert_eq!(ReferenceKind::Path.as_str(), "path");
    }

    #[test]
    fn nested_switching_off_pins_the_launch_runtime() {
        let mut schema = super::HookSchema {
            ..*hook_schema("identity-v1").expect("registered")
        };
        schema.nested_active = false;
        assert!(schema.admits_identity_provider("codex", Some("codex")));
        assert!(!schema.admits_identity_provider("claude", Some("codex")));
        assert!(!schema.admits_identity_provider("codex", None));
    }

    #[test]
    fn a_schema_admits_subagent_operations_only_with_a_declared_sequence_rule() {
        for schema in hook_schemas() {
            let operations =
                schema.allows(HookAction::SubagentStart) || schema.allows(HookAction::SubagentStop);
            assert_eq!(
                schema.allows(HookAction::SubagentStart),
                schema.allows(HookAction::SubagentStop),
                "{} admits both subagent operations or neither",
                schema.id
            );
            assert_eq!(
                operations,
                schema.subagent_sequence.is_some(),
                "{}",
                schema.id
            );
            assert_eq!(operations, schema.admits_subagents(), "{}", schema.id);
            assert_eq!(
                operations,
                !schema.subagent_providers.is_empty(),
                "{}",
                schema.id
            );
            assert_eq!(
                operations,
                !schema.subagent_fields.is_empty(),
                "{}",
                schema.id
            );
        }
    }

    #[test]
    fn a_missing_sequence_rule_or_provider_set_admits_no_subagents() {
        let mut schema = super::HookSchema {
            ..*hook_schema("identity-subagent-v1").expect("registered")
        };
        assert!(schema.admits_subagents());
        schema.subagent_sequence = None;
        assert!(!schema.admits_subagents());
        schema.subagent_sequence = Some(SubagentSequence::StopAfterStart);
        schema.subagent_providers = &[];
        assert!(!schema.admits_subagents());
    }

    #[test]
    fn a_stop_must_be_ordered_after_its_recorded_claim() {
        let rule = SubagentSequence::StopAfterStart;
        assert!(rule.admits_stop(None, 0));
        assert!(rule.admits_stop(Some(5), 6));
        assert!(!rule.admits_stop(Some(5), 5));
        assert!(!rule.admits_stop(Some(5), 4));
        assert!(rule.admits_stop(Some(0), 1));
        assert!(!rule.admits_stop(Some(u64::MAX), u64::MAX));
    }

    #[test]
    fn the_ancestry_matcher_admits_the_pty_tree_only() {
        for matcher in [AncestryMatcher::ManagedPtyDescendant, AncestryMatcher::CORE] {
            assert!(matcher.admits(PtyRelation::InTree));
            assert!(!matcher.admits(PtyRelation::Outside));
        }
        assert_eq!(PtyRelation::from_in_tree(true), PtyRelation::InTree);
        assert_eq!(PtyRelation::from_in_tree(false), PtyRelation::Outside);
        for schema in hook_schemas() {
            assert_eq!(schema.ancestry, AncestryMatcher::ManagedPtyDescendant);
        }
    }
}
