//! Composition of the small navigational opening prompt.
//!
//! The prompt is navigational, not knowledge: it carries the mission, the
//! non-negotiable safety rules, the active intent, the bundle/snapshot paths, an
//! intent-filtered table of contents, a three-line orientation, and the source
//! map. It **never** inlines bundle bodies — the agent pulls concepts by file.
//!
//! [`compose`] is a pure function so it is deterministic and unit-testable, and
//! so `--print-prompt` can show exactly what would be sent.

use std::fmt::Write as _;

use protocol::ConceptMeta;

use super::{Intent, SnapshotOrientation};

/// Inputs to [`compose`]. All borrowed so composition allocates only the result.
#[derive(Debug)]
pub(crate) struct ComposeParams<'a> {
    pub(crate) intent: Intent,
    pub(crate) request: Option<&'a str>,
    pub(crate) concepts: &'a [ConceptMeta],
    pub(crate) bundle_path: &'a str,
    pub(crate) snapshot_path: &'a str,
    pub(crate) orientation: &'a SnapshotOrientation,
    pub(crate) version: &'a str,
}

/// The non-negotiable safety rules inlined verbatim into every opening prompt.
///
/// These mirror the safety contract in `docs/knowledge/assistant/system.md` and
/// `docs/knowledge/safety/*.md`. They must hold even before the agent reads any
/// file, so they are inline rather than pulled.
const INLINE_SAFETY: &str = "\
- Never print, store, or infer secret values.
- Treat agent profile [env] values as secret-bearing.
- Explain config edits before applying them; preserve user edits unless asked to overwrite.
- Hooks are executable code: creation or modification requires \
explicit per-file confirmation, independent of --yes.
- Non-interactive contexts must quarantine proposed hook content instead of enabling it.
- Prefer structured --json inspection commands.
- Verify changes after applying them before claiming success.";

/// Inputs to [`compose_degraded`]. Subset of [`ComposeParams`]: no bundle.
#[derive(Debug)]
pub(crate) struct ComposeDegradedParams<'a> {
    pub(crate) intent: Intent,
    pub(crate) request: Option<&'a str>,
    pub(crate) snapshot_path: &'a str,
    pub(crate) orientation: &'a SnapshotOrientation,
    pub(crate) version: &'a str,
    /// Directory that contains `assistant/source-map.md` in a normal launch.
    /// For degraded mode this is informational only — no bundle was materialized.
    pub(crate) bundle_version_note: &'a str,
}

/// Compose the reduced navigational prompt for a degraded (`--degraded`) launch.
///
/// The bundle directory is absent, so this prompt carries only mission, safety,
/// intent, the snapshot path, and a source-map pointer. It omits the bundle
/// directory reference and the intent-filtered table of contents. The prompt
/// header explicitly states the session is degraded so the agent does not
/// silently assume a full knowledge base.
///
/// This function is pure and deterministic, so it is unit-testable without a
/// daemon.
#[must_use]
pub(crate) fn compose_degraded(params: &ComposeDegradedParams<'_>) -> String {
    let intent = params.intent;
    let mut prompt = String::with_capacity(1024);

    let _ = writeln!(prompt, "# Pohunek Assistant (degraded)\n");

    let _ = writeln!(prompt, "## Knowledge Status");
    let _ = writeln!(
        prompt,
        "knowledge: degraded — the knowledge bundle could not be materialized for version {}. \
         No bundle directory is available; no table of contents is provided. If precision matters, \
         read the source tree directly via the source map.\n",
        params.version
    );

    let _ = writeln!(prompt, "## Mission");
    let _ = writeln!(
        prompt,
        "You are the universal assistant for configuring, updating, troubleshooting, and \
         explaining pohunek (version {}).\n",
        params.version
    );

    let _ = writeln!(
        prompt,
        "## Safety (must hold even before you read anything)"
    );
    let _ = writeln!(prompt, "{INLINE_SAFETY}\n");

    let _ = writeln!(prompt, "## User Intent");
    let _ = writeln!(prompt, "intent: {}", intent.as_str());
    let _ = writeln!(
        prompt,
        "request: {}\n",
        params.request.unwrap_or("(none — orient and offer help)")
    );

    let _ = writeln!(prompt, "## Live Snapshot");
    let _ = writeln!(
        prompt,
        "Orientation: daemon={}, project={}, agent={}",
        params.orientation.daemon, params.orientation.project, params.orientation.agent
    );
    let _ = writeln!(prompt, "Full file: {}", params.snapshot_path);
    let _ = writeln!(
        prompt,
        "Read the full snapshot.json for doctor output, host capabilities, config scan, and \
         warnings. No knowledge bundle is available to cross-reference.\n"
    );

    let _ = writeln!(prompt, "## Source Map");
    let _ = writeln!(
        prompt,
        "{}assistant/source-map.md (in the repository source tree, if accessible) lists where \
         to verify implementation details when precision matters. The bundle was not \
         materialized, so you must navigate the source tree directly.\n",
        if params.bundle_version_note.is_empty() {
            String::new()
        } else {
            format!("[bundle version: {}] ", params.bundle_version_note)
        }
    );

    let _ = writeln!(prompt, "## First Step");
    let _ = write!(
        prompt,
        "Read the snapshot, identify the next concrete action, and proceed using documented \
         pohunek commands and file edits. The knowledge bundle is unavailable: rely on the \
         snapshot and any source-tree access. Verify changes before claiming they work."
    );

    prompt
}

/// Compose the navigational opening prompt for one assistant launch.
#[must_use]
pub(crate) fn compose(params: &ComposeParams<'_>) -> String {
    let intent = params.intent;
    let mut prompt = String::with_capacity(2048);

    let _ = writeln!(prompt, "# Pohunek Assistant\n");

    let _ = writeln!(prompt, "## Mission");
    let _ = writeln!(
        prompt,
        "You are the universal assistant for configuring, updating, troubleshooting, and \
         explaining pohunek (version {}).\n",
        params.version
    );

    let _ = writeln!(
        prompt,
        "## Safety (must hold even before you read anything)"
    );
    let _ = writeln!(prompt, "{INLINE_SAFETY}\n");

    let _ = writeln!(prompt, "## User Intent");
    let _ = writeln!(prompt, "intent: {}", intent.as_str());
    let _ = writeln!(
        prompt,
        "request: {}\n",
        params.request.unwrap_or("(none — orient and offer help)")
    );

    let _ = writeln!(prompt, "## Your Knowledge Base");
    let _ = writeln!(
        prompt,
        "Directory: {}   (version-shared cache for this binary)",
        params.bundle_path
    );
    let _ = writeln!(
        prompt,
        "Start at index.md. Navigate via index.md files and relative links between concepts. \
         Read only the concepts you need for this task; do not read the whole tree. The bundle \
         matches this binary ({}); when you also read the source tree via the source map, treat \
         the bundle as authoritative for documented behavior and watch the since / changed_in / \
         deprecated frontmatter for version skew.\n",
        params.version
    );

    let _ = writeln!(prompt, "## Relevant Concepts (intent: {})", intent.as_str());
    write_toc(&mut prompt, intent, params.concepts);
    let _ = writeln!(prompt);

    let _ = writeln!(prompt, "## Live Snapshot");
    let _ = writeln!(
        prompt,
        "Orientation: daemon={}, project={}, agent={}",
        params.orientation.daemon, params.orientation.project, params.orientation.agent
    );
    let _ = writeln!(prompt, "Full file: {}", params.snapshot_path);
    let _ = writeln!(
        prompt,
        "The three-line orientation is inline so you are not blind before reading; read the full \
         snapshot.json for doctor output, host capabilities, config scan, and warnings.\n"
    );

    let _ = writeln!(prompt, "## Source Map");
    let _ = writeln!(
        prompt,
        "{}/assistant/source-map.md lists where to verify implementation details against the \
         actual source tree when precision matters.\n",
        params.bundle_path
    );

    let _ = writeln!(prompt, "## First Step");
    let _ = write!(
        prompt,
        "Read the snapshot, open the relevant concepts, identify the next concrete action, and \
         proceed using documented pohunek commands and file edits. Verify changes before claiming \
         they work."
    );

    prompt
}

/// Write the intent-filtered table of contents (a list, not the content).
///
/// Concepts whose `intents` frontmatter contains the active intent are listed.
/// The `help` intent is broad: when no concept declares it, fall back to listing
/// every concept so the agent can orient rather than seeing an empty list.
fn write_toc(prompt: &mut String, intent: Intent, concepts: &[ConceptMeta]) {
    let wanted = intent.as_concept_intent();
    let mut listed = 0usize;
    for concept in concepts {
        if concept_matches(concept, wanted) {
            let _ = writeln!(prompt, "- {} — {}", concept.id, concept.description);
            listed += 1;
        }
    }

    if listed == 0 {
        // No concept is tagged for this intent; list everything so the agent is
        // never handed an empty table of contents.
        for concept in concepts {
            let _ = writeln!(prompt, "- {} — {}", concept.id, concept.description);
        }
    }
}

fn concept_matches(concept: &ConceptMeta, wanted: protocol::ConceptIntent) -> bool {
    concept
        .intents
        .as_ref()
        .is_some_and(|intents| intents.contains(&wanted))
}

#[cfg(test)]
mod tests {
    use protocol::{ConceptIntent, ConceptType};

    use super::*;

    fn orientation() -> SnapshotOrientation {
        SnapshotOrientation {
            daemon: "running".to_owned(),
            project: "ui".to_owned(),
            agent: "codex".to_owned(),
        }
    }

    fn concept(id: &str, intents: Option<Vec<ConceptIntent>>) -> ConceptMeta {
        ConceptMeta {
            r#type: ConceptType::Guide,
            id: id.to_owned(),
            title: id.to_owned(),
            description: format!("desc for {id}"),
            intents,
            since: None,
            changed_in: None,
            deprecated: None,
        }
    }

    // -----------------------------------------------------------------------
    // compose_degraded: structural guarantees
    // -----------------------------------------------------------------------

    #[test]
    fn degraded_prompt_contains_snapshot_path() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Help,
            request: None,
            snapshot_path: "/run/pohunek/assistant/abc/snapshot.json",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        assert!(
            prompt.contains("/run/pohunek/assistant/abc/snapshot.json"),
            "degraded prompt must carry the snapshot path; got:\n{prompt}"
        );
    }

    #[test]
    fn degraded_prompt_contains_source_map_pointer() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Help,
            request: None,
            snapshot_path: "/run/pohunek/assistant/abc/snapshot.json",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        assert!(
            prompt.contains("source-map.md"),
            "degraded prompt must carry the source-map pointer; got:\n{prompt}"
        );
    }

    #[test]
    fn degraded_prompt_does_not_contain_bundle_toc() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Setup,
            request: Some("configure the launcher"),
            snapshot_path: "/run/pohunek/assistant/abc/snapshot.json",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        // The "Relevant Concepts" section is ONLY in the full compose, not in degraded.
        assert!(
            !prompt.contains("Relevant Concepts"),
            "degraded prompt must not contain a bundle table of contents; got:\n{prompt}"
        );
        // No bundle directory reference either.
        assert!(
            !prompt.contains("Directory:"),
            "degraded prompt must not reference a bundle directory; got:\n{prompt}"
        );
    }

    #[test]
    fn degraded_prompt_header_signals_degraded() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Debug,
            request: None,
            snapshot_path: "/run/pohunek/assistant/abc/snapshot.json",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        assert!(
            prompt.contains("degraded"),
            "degraded prompt must include the word 'degraded' in the header; got:\n{prompt}"
        );
    }

    #[test]
    fn degraded_prompt_carries_inline_safety() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Update,
            request: None,
            snapshot_path: "/s.json",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        // The safety rules must be present even without a bundle.
        assert!(
            prompt.contains("Hooks are executable code"),
            "degraded prompt must still carry inline safety rules; got:\n{prompt}"
        );
    }

    #[test]
    fn inline_safety_hard_gates_hook_writes() {
        let full_prompt = compose(&ComposeParams {
            intent: Intent::Project,
            request: Some("install a hook"),
            concepts: &[concept(
                "safety/repo-pohunek",
                Some(vec![ConceptIntent::Project]),
            )],
            bundle_path: "/b",
            snapshot_path: "/s",
            orientation: &orientation(),
            version: "0.3.3",
        });
        let degraded_prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Project,
            request: Some("install a hook"),
            snapshot_path: "/s",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        for prompt in [&full_prompt, &degraded_prompt] {
            assert!(
                prompt.contains("explicit per-file confirmation, independent of --yes"),
                "prompt must hard-gate hook writes independent of --yes; got:\n{prompt}"
            );
            assert!(
                prompt.contains(
                    "Non-interactive contexts must quarantine proposed hook content instead of enabling it."
                ),
                "prompt must quarantine hook writes in non-interactive contexts; got:\n{prompt}"
            );
        }
    }

    #[test]
    fn degraded_prompt_carries_intent_and_request() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Project,
            request: Some("configure ci pipeline"),
            snapshot_path: "/s.json",
            orientation: &orientation(),
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        assert!(
            prompt.contains("intent: project"),
            "degraded prompt must carry the intent; got:\n{prompt}"
        );
        assert!(
            prompt.contains("configure ci pipeline"),
            "degraded prompt must carry the user request; got:\n{prompt}"
        );
    }

    #[test]
    fn degraded_prompt_carries_orientation() {
        let prompt = compose_degraded(&ComposeDegradedParams {
            intent: Intent::Help,
            request: None,
            snapshot_path: "/s.json",
            orientation: &SnapshotOrientation {
                daemon: "ok".to_owned(),
                project: "backend".to_owned(),
                agent: "claude".to_owned(),
            },
            version: "0.3.3",
            bundle_version_note: "0.3.3",
        });

        assert!(
            prompt.contains("daemon=ok, project=backend, agent=claude"),
            "degraded prompt must carry the 3-line orientation; got:\n{prompt}"
        );
    }

    // -----------------------------------------------------------------------
    // compose (full): pre-existing tests
    // -----------------------------------------------------------------------

    #[test]
    fn prompt_carries_sections_and_never_inlines_bodies() {
        let concepts = vec![concept("guides/setup", Some(vec![ConceptIntent::Setup]))];
        let prompt = compose(&ComposeParams {
            intent: Intent::Setup,
            request: Some("configure the launcher"),
            concepts: &concepts,
            bundle_path: "/cache/knowledge/v",
            snapshot_path: "/run/assistant/x/snapshot.json",
            orientation: &orientation(),
            version: "0.3.3",
        });

        assert!(prompt.contains("# Pohunek Assistant"));
        assert!(prompt.contains("intent: setup"));
        assert!(prompt.contains("request: configure the launcher"));
        assert!(prompt.contains("/cache/knowledge/v"));
        assert!(prompt.contains("/run/assistant/x/snapshot.json"));
        assert!(prompt.contains("daemon=running, project=ui, agent=codex"));
        assert!(prompt.contains("- guides/setup — desc for guides/setup"));
        // Navigational only: the safety hook rule is present.
        assert!(prompt.contains("Hooks are executable code"));
    }

    #[test]
    fn toc_filters_by_intent() {
        let concepts = vec![
            concept("guides/setup", Some(vec![ConceptIntent::Setup])),
            concept("guides/project", Some(vec![ConceptIntent::Project])),
        ];
        let prompt = compose(&ComposeParams {
            intent: Intent::Project,
            request: None,
            concepts: &concepts,
            bundle_path: "/b",
            snapshot_path: "/s",
            orientation: &orientation(),
            version: "0.3.3",
        });

        assert!(prompt.contains("guides/project"));
        assert!(!prompt.contains("guides/setup"));
    }

    #[test]
    fn toc_falls_back_to_all_when_no_match() {
        let concepts = vec![concept("concepts/arch", Some(vec![ConceptIntent::Setup]))];
        let prompt = compose(&ComposeParams {
            intent: Intent::Help,
            request: None,
            concepts: &concepts,
            bundle_path: "/b",
            snapshot_path: "/s",
            orientation: &orientation(),
            version: "0.3.3",
        });

        assert!(prompt.contains("concepts/arch"));
    }
}
