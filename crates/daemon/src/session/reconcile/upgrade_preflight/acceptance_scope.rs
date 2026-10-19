//! The acceptance scope of an at-risk preflight verdict.
//!
//! The scope is a digest of the facts a durable acceptance of the verdict
//! must stay bound to. The CLI journals it when an operator accepts the risk
//! and treats a later verdict as already accepted only when the daemon
//! reproduces the same digest, so every change to the session's identity, its
//! recovery reference or the verdict itself asks for consent again.
//!
//! The encoding is a cross-release contract: a daemon of the next release must
//! reproduce the digest of this one for identical facts. A different set of
//! facts or encoding needs a new scheme name; this one is frozen by the golden
//! digest in `tests/upgrade_preflight.rs`.

// Rust guideline compliant 2026-10-19

use pohunek_service_config::preflight::{
    Verdict, ACCEPTANCE_SCOPE_V1, CODE_GENERATION_MISSING, CODE_JOURNAL_SCHEMA_UNSUPPORTED,
    CODE_JOURNAL_UNREADABLE, CODE_PROTOCOL_INCOMPATIBLE, CODE_RUNTIME_IDENTITY_MISMATCH,
    CODE_RUNTIME_SOURCE_NOT_SERVED, CODE_WORKER_MARKER_LEGACY, CODE_WORKER_NOT_ADVANCED,
    CODE_WORKER_NOT_INITIALIZED,
};
use protocol::{BindingProvenance, RuntimeRef};
use sha2::{Digest, Sha256};

use crate::agent::host::LaunchPin;
use crate::session::SessionRecord;

/// Separates the digest input of this scheme from every other use of SHA-256.
const DOMAIN: &[u8] = b"pohunek.acceptance-scope\0v1\0";

/// Separates the digest of a native reference from the scope input that holds
/// it, so the reference itself never enters the report.
const NATIVE_REFERENCE_DOMAIN: &[u8] = b"pohunek.acceptance-scope.native-reference\0";

/// The codes whose scope survives the replacement daemon's startup unchanged
/// while the session stays at risk. A code outside the list gets no scope, so
/// a code added later is one-time until a differential case proves it.
const ELIGIBLE_CODES: &[&str] = &[
    CODE_GENERATION_MISSING,
    CODE_JOURNAL_SCHEMA_UNSUPPORTED,
    CODE_JOURNAL_UNREADABLE,
    CODE_PROTOCOL_INCOMPATIBLE,
    CODE_RUNTIME_IDENTITY_MISMATCH,
    CODE_RUNTIME_SOURCE_NOT_SERVED,
    CODE_WORKER_MARKER_LEGACY,
    CODE_WORKER_NOT_ADVANCED,
    CODE_WORKER_NOT_INITIALIZED,
];

/// Which list of the report a verdict belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListKind {
    /// A session that may own a live runtime.
    Live,
    /// A session whose runtime has ended.
    Stopped,
}

impl ListKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stopped => "stopped",
        }
    }
}

/// The scope of the at-risk verdict `verdict`/`code` over `candidate`, the
/// record state the judgement ended on, or `None` when nothing can durably
/// accept the verdict.
pub(super) fn scope_of(
    list: ListKind,
    candidate: &SessionRecord,
    verdict: Verdict,
    code: &str,
) -> Option<String> {
    if !verdict.at_risk() || !ELIGIBLE_CODES.contains(&code) {
        return None;
    }
    let mut encoder = Encoder(Sha256::new());
    encoder.0.update(DOMAIN);
    encoder.text(list.label());
    encoder.text(verdict.as_str());
    encoder.text(code);
    encoder.text(&candidate.session_id);
    encoder.optional(candidate.runtime.generation.as_deref());
    encoder.optional(candidate.runtime.worker_instance_id.as_deref());
    encoder.runtime_ref(&candidate.info.agent_base);
    match candidate.recovery.as_ref() {
        Some(binding) => {
            encoder.text("recovery");
            encoder.runtime_ref(&binding.agent_base);
            encoder.pin(&binding.launch_binding);
            encoder.optional_reference(binding.native_session_id.as_deref());
            encoder.optional_reference(binding.native_session_path.as_deref());
        }
        None => encoder.text("no-recovery"),
    }
    encoder.text(if candidate.info.capabilities.resume {
        "resume"
    } else {
        "no-resume"
    });
    encoder.text(if candidate.info.capabilities.fork {
        "fork"
    } else {
        "no-fork"
    });
    Some(format!(
        "{ACCEPTANCE_SCOPE_V1}:{}",
        hex(&encoder.0.finalize())
    ))
}

/// Feeds length-prefixed fields to the digest, so no two field sequences share
/// an input.
struct Encoder(Sha256);

impl Encoder {
    fn text(&mut self, value: &str) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value.as_bytes());
    }

    fn optional(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.text("some");
                self.text(value);
            }
            None => self.text("none"),
        }
    }

    /// A native reference enters the scope as a digest only.
    fn optional_reference(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                let mut digest = Sha256::new();
                digest.update(NATIVE_REFERENCE_DOMAIN);
                digest.update(value.as_bytes());
                self.text("some");
                self.text(&hex(&digest.finalize()));
            }
            None => self.text("none"),
        }
    }

    fn runtime_ref(&mut self, value: &RuntimeRef) {
        self.text(match value {
            RuntimeRef::Id(_) => "id",
            RuntimeRef::Historical(_) => "historical",
        });
        self.text(value.as_wire());
    }

    fn pin(&mut self, pin: &LaunchPin) {
        let LaunchPin::Pinned(binding) = pin else {
            self.text("unpinned");
            return;
        };
        self.text("pinned");
        self.text(binding.runtime_id.as_str());
        match &binding.provenance {
            BindingProvenance::Builtin {
                package,
                descriptor_digest,
            } => {
                self.text("builtin");
                match package {
                    Some(package) => {
                        self.text("some");
                        self.text(package.id.as_str());
                        self.text(package.version.as_str());
                    }
                    None => self.text("none"),
                }
                self.text(descriptor_digest.as_str());
            }
            BindingProvenance::Package {
                package,
                package_digest,
            } => {
                self.text("package");
                self.text(package.id.as_str());
                self.text(package.version.as_str());
                self.text(package_digest.as_str());
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}
