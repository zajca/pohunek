//! Completes the native recovery record of a session the schema migration
//! could not finish without the runtime registry.
//!
//! The migration marks a binding that lost its launch spec and belongs to a
//! non-built-in agent ([`ResumeBinding::native_launch_unresolved`]). When the
//! record is loaded, the spec is resolved by agent name, the same rule
//! `binding_native_launch` applies to a binding without a snapshot program. A
//! binding that cannot be resolved stays loadable: it logs a WARN, carries a
//! [`SessionWarningKind::NativeRecovery`] warning and is never resumed.

// Rust guideline compliant 2026-06-26

use protocol::{RuntimeRef, SessionCapabilities, SessionWarning, SessionWarningKind};
use tracing::warn;

use super::{ResumeBinding, SessionRecord, SessionRegistry};
use crate::agent::SessionRefKind;

/// What resolving one binding's launch spec did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NativeRepair {
    /// The binding was not marked, or already carries a spec.
    NotNeeded,
    /// The binding gained its launch spec.
    Repaired,
    /// The binding cannot recover natively; the reason is operator-facing.
    Unrepairable(String),
}

impl SessionRegistry {
    /// Resolves the launch spec of a marked binding by agent name.
    ///
    /// The flag is cleared only when the spec was set or was already present,
    /// so an unresolved binding is retried on the next load.
    pub(super) fn repair_native_launch(&self, binding: &mut ResumeBinding) -> NativeRepair {
        if !binding.native_launch_unresolved {
            return NativeRepair::NotNeeded;
        }
        if binding.native_launch.is_some() {
            binding.native_launch_unresolved = false;
            return NativeRepair::NotNeeded;
        }
        let resolved = match self.inner.profiles.resolve_agent(&binding.agent) {
            Ok(resolved) => resolved,
            Err(error) => {
                return NativeRepair::Unrepairable(format!(
                    "agent `{}` does not resolve ({})",
                    binding.agent, error.code
                ));
            }
        };
        if RuntimeRef::from(resolved.base.clone()) != binding.agent_base {
            return NativeRepair::Unrepairable(format!(
                "agent `{}` no longer runs on the runtime this session was started with",
                binding.agent
            ));
        }
        let Some(launch) = resolved.native_launch() else {
            return NativeRepair::Unrepairable(format!(
                "agent `{}` declares no native resume",
                binding.agent
            ));
        };
        let reference_present = match launch.reference_kind() {
            SessionRefKind::Id => binding.native_session_id.is_some(),
            SessionRefKind::Path => binding.native_session_path.is_some(),
        };
        if !reference_present {
            return NativeRepair::Unrepairable(format!(
                "the native reference kind of agent `{}` does not match the stored reference",
                binding.agent
            ));
        }
        binding.native_launch = Some(launch);
        binding.native_launch_unresolved = false;
        NativeRepair::Repaired
    }

    /// Repairs the recovery binding of `record` and the separately persisted
    /// resume `binding`, then aligns the frozen capabilities and the warning
    /// with the result.
    ///
    /// Both bindings resolve through the same rule, so they stay equal and the
    /// merge of the two never reports a shape mismatch.
    pub(super) fn repair_native_recovery(
        &self,
        record: &mut SessionRecord,
        mut binding: Option<&mut ResumeBinding>,
    ) {
        let mut outcomes = Vec::new();
        if let Some(recovery) = record.recovery.as_mut() {
            outcomes.push(self.repair_native_launch(recovery));
        }
        if let Some(binding) = binding.as_deref_mut() {
            outcomes.push(self.repair_native_launch(binding));
        }
        let unrepairable = outcomes.iter().find_map(|outcome| match outcome {
            NativeRepair::Unrepairable(reason) => Some(reason.clone()),
            NativeRepair::NotNeeded | NativeRepair::Repaired => None,
        });
        record
            .info
            .warnings
            .retain(|warning| warning.kind != SessionWarningKind::NativeRecovery);
        if let Some(reason) = unrepairable {
            warn!(
                name: "reconcile.native_recovery.unrepairable",
                session_id = %record.session_id,
                agent = %record.info.agent,
                reason = %reason,
                "stored native recovery record cannot be completed; the session cannot resume or fork natively"
            );
            let detail = native_reference_detail(record, binding.as_deref());
            record.info.warnings.push(SessionWarning {
                kind: SessionWarningKind::NativeRecovery,
                message: format!(
                    "native recovery is unavailable: {reason}. Start a new session and resume \
                     the native conversation in it."
                ),
                detail,
            });
            return;
        }
        if outcomes.contains(&NativeRepair::Repaired) {
            let launch = record
                .recovery
                .as_ref()
                .and_then(|recovery| recovery.native_launch.as_ref())
                .or_else(|| binding.as_deref().and_then(|b| b.native_launch.as_ref()));
            record.info.capabilities = SessionCapabilities {
                resume: launch.is_some(),
                fork: launch.is_some_and(crate::agent::NativeSessionLaunch::supports_fork),
            };
        }
    }
}

/// The native reference the operator resumes manually, when one is stored.
fn native_reference_detail(
    record: &SessionRecord,
    binding: Option<&ResumeBinding>,
) -> Option<String> {
    let candidates = [record.recovery.as_ref(), binding];
    candidates.into_iter().flatten().find_map(|candidate| {
        match (&candidate.native_session_id, &candidate.native_session_path) {
            (Some(id), _) => Some(format!("native session id: {id}")),
            (None, Some(path)) => Some(format!("native session path: {path}")),
            (None, None) => None,
        }
    })
}
