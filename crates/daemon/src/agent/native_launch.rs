//! Typed, shell-free native-session launch specification.
//!
//! A [`NativeSessionLaunch`] declares how an agent CLI resumes (and optionally
//! forks) one of its own conversations: the kind of native reference it
//! consumes plus a literal argv with exactly one reference slot per operation.
//! Compiled adapters, host profiles, persisted resume bindings, recovery, and
//! explicit fork all share this one representation, so generic code never
//! branches on a provider name.
//!
//! The reference is a [`NativeArg::Reference`] token, never a substring of a
//! larger string: rendering substitutes the validated [`SessionRef`] value as
//! one complete argv element, so whitespace and shell metacharacters in it can
//! never split, expand, or turn into an option.

use protocol::{ErrorClass, ProtocolError};
use serde::{Deserialize, Serialize};

use super::{AssignedReference, SessionRef, SessionRefKind};

/// Whole-token sentinel that marks the reference slot in a profile template.
pub const REFERENCE_PLACEHOLDER: &str = "{reference}";

/// One element of a native launch argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeArg {
    /// A fixed argument passed through verbatim.
    Literal(String),
    /// The slot filled with the validated native session reference.
    Reference,
}

/// Why a native launch template or spec is invalid.
///
/// Variants carry the zero-based token index and never the token text, so a
/// diagnostic cannot echo argument values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeLaunchError {
    /// The template has no tokens.
    #[error("the argument list is empty")]
    EmptyTemplate,
    /// A literal argument is the empty string.
    #[error("argument {index} is an empty literal")]
    EmptyLiteral {
        /// Zero-based token index.
        index: usize,
    },
    /// A literal argument contains a control character (including NUL).
    #[error("argument {index} contains a control character")]
    ControlCharacter {
        /// Zero-based token index.
        index: usize,
    },
    /// A literal contains `{` or `}`: the reference placeholder must be a whole
    /// token and no other placeholder exists.
    #[error(
        "argument {index} contains braces; only a whole-token `{{reference}}` placeholder is supported"
    )]
    BracesInLiteral {
        /// Zero-based token index.
        index: usize,
    },
    /// No token is the reference placeholder.
    #[error("the argument list has no `{{reference}}` placeholder")]
    MissingReference,
    /// More than one token is the reference placeholder.
    #[error("the argument list has more than one `{{reference}}` placeholder")]
    DuplicateReference,
    /// An assigned reference needs an id-kind spec: core can generate an id but
    /// not a path.
    #[error("an assigned reference requires an id reference kind")]
    AssignedRequiresId,
}

/// A validated argv fragment with exactly one reference slot.
///
/// Invariants, enforced by [`NativeArgs::new`] and by deserialization: at least
/// one token, exactly one [`NativeArg::Reference`], and every literal is
/// non-empty, free of control characters, and free of braces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<NativeArg>", into = "Vec<NativeArg>")]
pub struct NativeArgs(Vec<NativeArg>);

impl NativeArgs {
    /// Validate and wrap an argv fragment.
    ///
    /// # Errors
    ///
    /// Returns the first violated invariant as a [`NativeLaunchError`].
    pub fn new(args: Vec<NativeArg>) -> Result<Self, NativeLaunchError> {
        if args.is_empty() {
            return Err(NativeLaunchError::EmptyTemplate);
        }
        let mut references = 0_usize;
        for (index, arg) in args.iter().enumerate() {
            match arg {
                NativeArg::Reference => references += 1,
                NativeArg::Literal(value) => {
                    if value.is_empty() {
                        return Err(NativeLaunchError::EmptyLiteral { index });
                    }
                    if value.chars().any(char::is_control) {
                        return Err(NativeLaunchError::ControlCharacter { index });
                    }
                    if value.contains(['{', '}']) {
                        return Err(NativeLaunchError::BracesInLiteral { index });
                    }
                }
            }
        }
        match references {
            0 => Err(NativeLaunchError::MissingReference),
            1 => Ok(Self(args)),
            _ => Err(NativeLaunchError::DuplicateReference),
        }
    }

    /// Parse a profile template, mapping each whole-token `{reference}` to
    /// [`NativeArg::Reference`] and every other token to a literal.
    ///
    /// # Errors
    ///
    /// Returns a [`NativeLaunchError`] when the template violates an invariant
    /// of [`NativeArgs::new`], including an embedded or unknown placeholder.
    pub fn from_template<S: AsRef<str>>(tokens: &[S]) -> Result<Self, NativeLaunchError> {
        Self::new(
            tokens
                .iter()
                .map(|token| {
                    let token = token.as_ref();
                    if token == REFERENCE_PLACEHOLDER {
                        NativeArg::Reference
                    } else {
                        NativeArg::Literal(token.to_owned())
                    }
                })
                .collect(),
        )
    }

    /// The validated tokens.
    #[must_use]
    pub fn as_slice(&self) -> &[NativeArg] {
        &self.0
    }

    /// Substitute `reference` for the reference slot, yielding one argv element
    /// per token.
    #[must_use]
    pub fn render(&self, reference: &str) -> Vec<String> {
        self.0
            .iter()
            .map(|arg| match arg {
                NativeArg::Literal(value) => value.clone(),
                NativeArg::Reference => reference.to_owned(),
            })
            .collect()
    }
}

impl TryFrom<Vec<NativeArg>> for NativeArgs {
    type Error = NativeLaunchError;

    fn try_from(args: Vec<NativeArg>) -> Result<Self, Self::Error> {
        Self::new(args)
    }
}

impl From<NativeArgs> for Vec<NativeArg> {
    fn from(args: NativeArgs) -> Self {
        args.0
    }
}

/// How an agent resumes and optionally forks its native conversations.
///
/// Resume is mandatory: a fork is only expressible alongside the resume that
/// declares the reference kind. The spec is frozen into persisted session
/// snapshots so recovery uses the launch-time shape even after the host
/// profile changes.
///
/// Deserialization runs the same invariants as [`NativeSessionLaunch::with_assigned`],
/// so a stored spec that pairs an assignment with a non-id reference kind does
/// not load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StoredNativeSessionLaunch")]
pub struct NativeSessionLaunch {
    reference_kind: SessionRefKind,
    resume_args: NativeArgs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fork_args: Option<NativeArgs>,
    /// Present when core generates the reference and passes it at launch
    /// instead of waiting for an integration report. Boxed: most specs have
    /// none, and this keeps the persisted binding small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assigned: Option<Box<AssignedReference>>,
}

/// Plain stored form of a [`NativeSessionLaunch`], validated on conversion.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredNativeSessionLaunch {
    reference_kind: SessionRefKind,
    resume_args: NativeArgs,
    #[serde(default)]
    fork_args: Option<NativeArgs>,
    #[serde(default)]
    assigned: Option<Box<AssignedReference>>,
}

impl TryFrom<StoredNativeSessionLaunch> for NativeSessionLaunch {
    type Error = NativeLaunchError;

    fn try_from(stored: StoredNativeSessionLaunch) -> Result<Self, Self::Error> {
        let spec = Self::new(stored.reference_kind, stored.resume_args, stored.fork_args);
        match stored.assigned {
            Some(assigned) => spec.with_assigned(*assigned),
            None => Ok(spec),
        }
    }
}

impl NativeSessionLaunch {
    /// Build a spec from validated resume and optional fork argv fragments.
    #[must_use]
    pub fn new(
        reference_kind: SessionRefKind,
        resume_args: NativeArgs,
        fork_args: Option<NativeArgs>,
    ) -> Self {
        Self {
            reference_kind,
            resume_args,
            fork_args,
            assigned: None,
        }
    }

    /// Marks the reference as core-assigned at launch.
    ///
    /// # Errors
    ///
    /// Returns [`NativeLaunchError::AssignedRequiresId`] unless the spec's
    /// reference kind is [`SessionRefKind::Id`].
    pub fn with_assigned(mut self, assigned: AssignedReference) -> Result<Self, NativeLaunchError> {
        if self.reference_kind != SessionRefKind::Id {
            return Err(NativeLaunchError::AssignedRequiresId);
        }
        self.assigned = Some(Box::new(assigned));
        Ok(self)
    }

    /// Build a spec from resume and optional fork templates, applying the
    /// whole-token `{reference}` rules of [`NativeArgs::from_template`].
    ///
    /// # Errors
    ///
    /// Returns the first [`NativeLaunchError`] of either template.
    pub fn from_templates<S: AsRef<str>>(
        reference_kind: SessionRefKind,
        resume: &[S],
        fork: Option<&[S]>,
    ) -> Result<Self, NativeLaunchError> {
        Ok(Self::new(
            reference_kind,
            NativeArgs::from_template(resume)?,
            fork.map(NativeArgs::from_template).transpose()?,
        ))
    }

    /// The kind of native reference both operations consume.
    #[must_use]
    pub fn reference_kind(&self) -> SessionRefKind {
        self.reference_kind
    }

    /// The launch-time assignment of the reference, when core assigns it.
    #[must_use]
    pub fn assigned(&self) -> Option<&AssignedReference> {
        self.assigned.as_deref()
    }

    /// The resume argv fragment.
    #[must_use]
    pub fn resume_args(&self) -> &NativeArgs {
        &self.resume_args
    }

    /// The fork argv fragment, when the agent supports native fork.
    #[must_use]
    pub fn fork_args(&self) -> Option<&NativeArgs> {
        self.fork_args.as_ref()
    }

    /// Whether the agent supports native fork.
    #[must_use]
    pub fn supports_fork(&self) -> bool {
        self.fork_args.is_some()
    }

    /// Build the resume argv fragment for `reference`.
    ///
    /// # Errors
    ///
    /// Returns `native_reference_kind_mismatch` when `reference` is not of this
    /// spec's kind, so the kind-specific validation guard stays authoritative.
    pub fn resume_argv(&self, reference: &SessionRef) -> Result<Vec<String>, ProtocolError> {
        self.ensure_kind(reference)?;
        Ok(self.resume_args.render(reference.value()))
    }

    /// Build the fork argv fragment for `reference`.
    ///
    /// # Errors
    ///
    /// Returns `agent_fork_unsupported` when the spec declares no fork, or
    /// `native_reference_kind_mismatch` when `reference` is of another kind.
    pub fn fork_argv(&self, reference: &SessionRef) -> Result<Vec<String>, ProtocolError> {
        let fork_args = self
            .fork_args
            .as_ref()
            .ok_or_else(ProtocolError::agent_fork_unsupported)?;
        self.ensure_kind(reference)?;
        Ok(fork_args.render(reference.value()))
    }

    fn ensure_kind(&self, reference: &SessionRef) -> Result<(), ProtocolError> {
        if reference.kind() == self.reference_kind {
            Ok(())
        } else {
            Err(ProtocolError::new(
                ErrorClass::Runtime,
                "native_reference_kind_mismatch",
                "native session reference kind does not match the launch spec",
                None,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(value: &str) -> NativeArg {
        NativeArg::Literal(value.to_owned())
    }

    #[test]
    fn template_maps_the_whole_token_sentinel_to_the_reference_slot() {
        let args = NativeArgs::from_template(&["--session", "{reference}"]).expect("valid");
        assert_eq!(
            args.as_slice(),
            [literal("--session"), NativeArg::Reference]
        );
        assert_eq!(args.render("id-1"), vec!["--session", "id-1"]);
    }

    #[test]
    fn template_rejects_every_invalid_shape() {
        let empty: [&str; 0] = [];
        assert_eq!(
            NativeArgs::from_template(&empty),
            Err(NativeLaunchError::EmptyTemplate)
        );
        assert_eq!(
            NativeArgs::from_template(&["--session"]),
            Err(NativeLaunchError::MissingReference)
        );
        assert_eq!(
            NativeArgs::from_template(&["{reference}", "{reference}"]),
            Err(NativeLaunchError::DuplicateReference)
        );
        assert_eq!(
            NativeArgs::from_template(&["--session={reference}"]),
            Err(NativeLaunchError::BracesInLiteral { index: 0 })
        );
        assert_eq!(
            NativeArgs::from_template(&["--session", "{ref}", "{reference}"]),
            Err(NativeLaunchError::BracesInLiteral { index: 1 })
        );
        assert_eq!(
            NativeArgs::from_template(&["", "{reference}"]),
            Err(NativeLaunchError::EmptyLiteral { index: 0 })
        );
        assert_eq!(
            NativeArgs::from_template(&["bad\0arg", "{reference}"]),
            Err(NativeLaunchError::ControlCharacter { index: 0 })
        );
    }

    #[test]
    fn diagnostics_never_echo_template_text() {
        let error = NativeArgs::from_template(&["--token=hunter2-{reference}"])
            .expect_err("embedded placeholder");
        assert!(!error.to_string().contains("hunter2"));
    }

    #[test]
    fn render_keeps_hostile_references_as_one_element() {
        let args = NativeArgs::from_template(&["resume", "{reference}"]).expect("valid");
        let hostile = "a b;$(touch x)|`y` --flag 'q' \"z\"";
        assert_eq!(
            args.render(hostile),
            vec!["resume".to_owned(), hostile.to_owned()]
        );
    }

    #[test]
    fn deserialization_enforces_the_same_invariants() {
        let ok: NativeArgs =
            serde_json::from_str(r#"[{"literal":"--resume"},"reference"]"#).expect("valid");
        assert_eq!(ok.render("x"), vec!["--resume", "x"]);
        for bad in [
            r"[]",
            r#"[{"literal":"--resume"}]"#,
            r#"["reference","reference"]"#,
            r#"[{"literal":""},"reference"]"#,
            r#"[{"literal":"{reference}"},"reference"]"#,
        ] {
            assert!(
                serde_json::from_str::<NativeArgs>(bad).is_err(),
                "must reject {bad}"
            );
        }
    }

    fn launch(fork: Option<&[&str]>) -> NativeSessionLaunch {
        NativeSessionLaunch::new(
            SessionRefKind::Path,
            NativeArgs::from_template(&["--session", "{reference}"]).expect("resume"),
            fork.map(|tokens| NativeArgs::from_template(tokens).expect("fork")),
        )
    }

    #[test]
    fn pi_shaped_spec_builds_resume_and_fork_argv() {
        let spec = launch(Some(&["--fork", "{reference}"]));
        let reference = SessionRef::path("/work/a b/$(x);.jsonl").expect("path reference");
        assert_eq!(
            spec.resume_argv(&reference).expect("resume"),
            vec!["--session", "/work/a b/$(x);.jsonl"]
        );
        assert_eq!(
            spec.fork_argv(&reference).expect("fork"),
            vec!["--fork", "/work/a b/$(x);.jsonl"]
        );
    }

    #[test]
    fn fork_is_unsupported_without_fork_args() {
        let spec = launch(None);
        let reference = SessionRef::path("/work/s.jsonl").expect("path reference");
        assert!(!spec.supports_fork());
        assert_eq!(
            spec.fork_argv(&reference).expect_err("no fork").code,
            "agent_fork_unsupported"
        );
    }

    #[test]
    fn a_reference_of_the_wrong_kind_is_refused() {
        let spec = launch(Some(&["--fork", "{reference}"]));
        let reference = SessionRef::id("native-1").expect("id reference");
        assert_eq!(
            spec.resume_argv(&reference).expect_err("kind").code,
            "native_reference_kind_mismatch"
        );
        assert_eq!(
            spec.fork_argv(&reference).expect_err("kind").code,
            "native_reference_kind_mismatch"
        );
    }

    #[test]
    fn spec_roundtrips_through_json_and_rejects_corruption() {
        let spec = launch(Some(&["--fork", "{reference}"]));
        let encoded = serde_json::to_string(&spec).expect("encode");
        let decoded: NativeSessionLaunch = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, spec);

        let without_fork = launch(None);
        let encoded = serde_json::to_string(&without_fork).expect("encode");
        assert!(!encoded.contains("fork_args"));
        assert_eq!(
            serde_json::from_str::<NativeSessionLaunch>(&encoded).expect("decode"),
            without_fork
        );

        let corrupt = r#"{"reference_kind":"id","resume_args":["reference","reference"]}"#;
        serde_json::from_str::<NativeSessionLaunch>(corrupt)
            .expect_err("a spec with two reference slots must not decode");
    }

    fn assignment() -> AssignedReference {
        AssignedReference::new(
            NativeArgs::from_template(&["--session-id", "{reference}"]).expect("template"),
            crate::agent::ReferenceExistence::Unchecked,
        )
    }

    #[test]
    fn an_assignment_needs_an_id_reference_kind() {
        assert_eq!(
            launch(None).with_assigned(assignment()),
            Err(NativeLaunchError::AssignedRequiresId)
        );
        let id_spec = NativeSessionLaunch::new(
            SessionRefKind::Id,
            NativeArgs::from_template(&["--session", "{reference}"]).expect("resume"),
            None,
        )
        .with_assigned(assignment())
        .expect("an id spec accepts an assignment");
        assert!(id_spec.assigned().is_some());
    }

    #[test]
    fn an_assignment_roundtrips_and_a_plain_spec_omits_it() {
        let spec = NativeSessionLaunch::new(
            SessionRefKind::Id,
            NativeArgs::from_template(&["--session", "{reference}"]).expect("resume"),
            None,
        )
        .with_assigned(assignment())
        .expect("assigned");
        let encoded = serde_json::to_string(&spec).expect("encode");
        assert_eq!(
            serde_json::from_str::<NativeSessionLaunch>(&encoded).expect("decode"),
            spec
        );
        let plain = serde_json::to_string(&launch(None)).expect("encode");
        assert!(!plain.contains("assigned"));
    }

    #[test]
    fn a_stored_spec_pairing_an_assignment_with_a_path_kind_does_not_decode() {
        let corrupt = r#"{"reference_kind":"path","resume_args":[{"literal":"--session"},"reference"],"assigned":{"launch_args":[{"literal":"--session-id"},"reference"],"existence":{"check":"none"}}}"#;
        serde_json::from_str::<NativeSessionLaunch>(corrupt)
            .expect_err("an assignment needs an id reference kind");
        let valid = corrupt.replace("\"path\"", "\"id\"");
        assert!(serde_json::from_str::<NativeSessionLaunch>(&valid)
            .expect("an id spec with an assignment decodes")
            .assigned()
            .is_some());
    }
}
