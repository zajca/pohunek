//! Rendering the review prompt and dispatching a review as a new
//! same-worktree session.

// Rust guideline compliant 2026-07-19

use std::collections::BTreeMap;
use std::path::Path;

use protocol::{SessionInfo, SessionNewParams, SessionNewResult};

use super::model::{now_rfc3339, Review, ReviewComment};
use super::store::ReviewStore;
use crate::{
    create_session_with_options, render_prompt, ConnectionOptions, CoreError, HostConfig,
    PromptProvider,
};

/// Session metadata key recording which review dispatched a session.
pub const REVIEW_SOURCE_KEY: &str = "review.source";
/// Session metadata key recording when a review was dispatched, RFC3339.
pub const REVIEW_DISPATCHED_AT_KEY: &str = "review.dispatched_at";

/// Prefix shared by every `link.*` session metadata key
/// (`crates/prompt/src/link.rs`'s `LINK_*_KEY` constants). Copied verbatim
/// from the source session onto a dispatched review session so the review
/// session stays linked to the same provider item as its source.
const LINK_METADATA_PREFIX: &str = "link.";

/// Reads and renders `<config_dir>/prompts/review.tmpl` for `review`.
///
/// Reads the host config directory directly
/// (`pohunek_paths::config_home()`/`pohunek`/`prompts/review.tmpl`),
/// bypassing the per-project `ProjectConfigResolver` in-repo-shadows-host
/// layered lookup that `project.action` templates use: review dispatch is a
/// GUI-global feature with no project/repo context available at render time,
/// so there is nothing to shadow the host template with. This is a deliberate
/// design choice (`.agent-context/d6-context.md` §7), not an oversight.
///
/// `source_description` is a human-readable summary of what was reviewed
/// (e.g. `"session abc123 worktree diff vs main"` or `"PR #42"`), supplied by
/// the caller because deriving it requires data this type does not itself
/// hold (a live `SessionInfo`'s short id/base, or PR metadata).
///
/// # Errors
///
/// Returns [`CoreError::MissingReviewTemplate`] when `review.tmpl` is absent —
/// no silent default template; the operator should run `pohunek setup`.
/// Returns [`CoreError::MissingEnv`] when neither `XDG_CONFIG_HOME` nor `HOME`
/// resolves. Returns [`CoreError::Prompt`] when the template references a
/// variable outside `${branch}`/`${source}`/`${comments}`/`${comment_count}`.
pub fn render_review_prompt(
    review: &Review,
    source_description: &str,
) -> Result<String, CoreError> {
    let config_dir = pohunek_paths::config_home()
        .map_err(|_source| CoreError::MissingEnv {
            var: "XDG_CONFIG_HOME or HOME".to_owned(),
        })?
        .join(pohunek_paths::APP_DIR);
    render_review_prompt_from_config_dir(review, source_description, &config_dir)
}

/// Same as [`render_review_prompt`], but with the host config directory
/// (normally `pohunek_paths::config_home()/pohunek`) passed in explicitly.
///
/// Split out purely so the template-resolution success/failure paths are
/// unit-testable against a temp directory without mutating process-wide
/// `XDG_CONFIG_HOME`/`HOME` env vars (which would require serializing tests
/// against each other). [`render_review_prompt`] is the only public entry
/// point; this stays private to the module.
fn render_review_prompt_from_config_dir(
    review: &Review,
    source_description: &str,
    config_dir: &Path,
) -> Result<String, CoreError> {
    let template_path = config_dir.join("prompts").join("review.tmpl");
    let template = std::fs::read_to_string(&template_path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            CoreError::MissingReviewTemplate {
                path: template_path.clone(),
            }
        } else {
            CoreError::ReviewTemplateIo {
                path: template_path.clone(),
                source,
            }
        }
    })?;
    let context_json = review_context_json(review, source_description);
    Ok(render_prompt(
        &template,
        PromptProvider::Review,
        review.id.as_str(),
        &context_json,
    )?)
}

fn review_context_json(review: &Review, source_description: &str) -> String {
    let comments = review
        .comments
        .iter()
        .map(render_comment_block)
        .collect::<Vec<_>>()
        .join("\n\n");
    serde_json::json!({
        "branch": review.branch,
        "source": source_description,
        "comments": comments,
        "comment_count": review.comments.len(),
    })
    .to_string()
}

fn render_comment_block(comment: &ReviewComment) -> String {
    format!(
        "{}:{} ({}): {}",
        comment.path,
        comment.line,
        comment.side.as_str(),
        comment.text
    )
}

/// Parameters for [`dispatch_review`] other than the review being dispatched.
///
/// Grouped into one struct (mirroring [`crate::PromptLaunchParams`] and
/// [`crate::ProviderLaunchParams`]) rather than passed as separate
/// arguments, keeping the function signature small.
#[derive(Debug)]
pub struct ReviewDispatchParams<'a> {
    /// Host to dispatch the new session on.
    pub config: &'a HostConfig,
    /// Store the dispatched review is persisted to.
    pub store: &'a ReviewStore,
    /// Current `session.inspect` result for the review's source session.
    pub session_info: &'a SessionInfo,
    /// Agent profile to run the dispatched session as. `Some` overrides
    /// `session_info.agent` with the operator's pick from the Review tab's
    /// dispatch modal agent picker; `None` falls back to
    /// `session_info.agent`, reusing the source session's own profile.
    pub agent: Option<String>,
    /// Output of [`render_review_prompt`], rendered by the caller so a
    /// template error surfaces before any session is created.
    pub rendered_prompt: String,
    /// Initial terminal width in columns for the dispatched session.
    pub cols: u16,
    /// Initial terminal height in rows for the dispatched session.
    pub rows: u16,
    /// Connection options for the `session.new` call.
    pub options: ConnectionOptions,
}

/// Dispatches `review` as a new session in its source session's SAME
/// worktree, then marks the review dispatched and persists it.
///
/// `params.session_info` must be the *current* `session.inspect` result for
/// the review's source session — its `worktree_path` supplies the new
/// session's `cwd`, with no local path guessing. The dispatched session's
/// agent profile is `params.agent` when given (the operator's pick from the
/// dispatch modal), otherwise `session_info.agent` — reusing the source
/// session's own profile by default, but overridable.
///
/// This intentionally omits `project`/`repo`/`branch` from the `session.new`
/// call: those would make the daemon mint a *new* worktree binding, which is
/// impossible while the source session's worktree still exists (git refuses
/// a second checkout of the same branch — NEXT.md D.6 decision 2). `cwd`
/// alone launches the new session in place, reusing the existing checkout.
///
/// `link.*` metadata keys present on `params.session_info` are copied
/// verbatim onto the new session, alongside `review.source` (this review's
/// id) and `review.dispatched_at` (RFC3339 now).
///
/// # Errors
///
/// Returns [`CoreError::ReviewSessionMissingWorktree`] when `session_info` has
/// no bound worktree. Returns the `session.new` error unchanged when the
/// daemon refuses — `review` and its on-disk draft are left untouched, since
/// this function only mutates `review`/persists it *after* `session.new`
/// succeeds. Returns [`CoreError::ReviewStore`] when the successful dispatch
/// cannot be persisted; the daemon session already exists at that point, but
/// the atomic store write means the on-disk review file is unaffected by the
/// failed save (still the pre-dispatch draft), never a half-written file.
pub async fn dispatch_review(
    review: &mut Review,
    params: ReviewDispatchParams<'_>,
) -> Result<SessionNewResult, CoreError> {
    let ReviewDispatchParams {
        config,
        store,
        session_info,
        agent,
        rendered_prompt,
        cols,
        rows,
        options,
    } = params;
    let Some(worktree_path) = session_info.worktree_path.clone() else {
        return Err(CoreError::ReviewSessionMissingWorktree {
            session_id: session_info.id.clone(),
        });
    };

    let mut metadata = copied_link_metadata(&session_info.metadata);
    metadata.insert(REVIEW_SOURCE_KEY.to_owned(), review.id.as_str().to_owned());
    metadata.insert(REVIEW_DISPATCHED_AT_KEY.to_owned(), now_rfc3339());

    let new_params = SessionNewParams {
        agent: agent.unwrap_or_else(|| session_info.agent.clone()),
        name: None,
        cwd: Some(worktree_path),
        cols,
        rows,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: Some(rendered_prompt),
        metadata,
    };

    let created = create_session_with_options(config, new_params, options).await?;

    review.mark_dispatched(created.session.id.clone());
    store.save(review)?;

    Ok(created)
}

fn copied_link_metadata(source: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    source
        .iter()
        .filter(|(key, _)| key.starts_with(LINK_METADATA_PREFIX))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{render_comment_block, render_review_prompt_from_config_dir, review_context_json};
    use crate::review::model::{Review, ReviewComment, ReviewSide, ReviewSource};
    use crate::{CoreError, HostId};
    use protocol::SessionId;

    fn temp_config_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-gui-core-review-dispatch-{tag}-{}",
            std::process::id()
        ))
    }

    fn sample_review() -> Review {
        let mut review = Review::new(
            ReviewSource::Session {
                host_id: HostId::new("host-1"),
                session_id: SessionId("s-1".to_owned()),
            },
            "project-1",
            "feature/diff-review",
        );
        review.add_comment(ReviewComment::new(
            "src/lib.rs",
            ReviewSide::New,
            10,
            "fix this",
        ));
        review.add_comment(ReviewComment::new(
            "src/lib.rs",
            ReviewSide::Old,
            20,
            "remove dead code",
        ));
        review
    }

    #[test]
    fn render_comment_block_matches_the_documented_format() {
        let comment = ReviewComment::new("src/lib.rs", ReviewSide::New, 10, "fix this");
        assert_eq!(
            render_comment_block(&comment),
            "src/lib.rs:10 (new): fix this"
        );
    }

    #[test]
    fn review_context_json_carries_branch_source_comments_and_count() {
        let review = sample_review();

        let context_json = review_context_json(&review, "PR #42");
        let context: serde_json::Value = serde_json::from_str(&context_json).expect("valid json");

        assert_eq!(context["branch"], "feature/diff-review");
        assert_eq!(context["source"], "PR #42");
        assert_eq!(context["comment_count"], 2);
        assert_eq!(
            context["comments"],
            "src/lib.rs:10 (new): fix this\n\nsrc/lib.rs:20 (old): remove dead code"
        );
    }

    #[test]
    fn review_context_json_uses_empty_string_for_no_comments() {
        let review = Review::new(
            ReviewSource::PullRequest {
                host_id: HostId::new("host-1"),
                pr_number: 1,
            },
            "project-1",
            "feature/x",
        );

        let context_json = review_context_json(&review, "PR #1");
        let context: serde_json::Value = serde_json::from_str(&context_json).expect("valid json");

        assert_eq!(context["comments"], "");
        assert_eq!(context["comment_count"], 0);
    }

    #[test]
    fn render_review_prompt_succeeds_when_the_template_file_exists() {
        let config_dir = temp_config_dir("template-present");
        let prompts_dir = config_dir.join("prompts");
        std::fs::create_dir_all(&prompts_dir).expect("create prompts dir");
        std::fs::write(
            prompts_dir.join("review.tmpl"),
            "Review of ${source} on branch ${branch} (${comment_count} comments):\n${comments}\n",
        )
        .expect("write review.tmpl");
        let review = sample_review();

        let rendered = render_review_prompt_from_config_dir(&review, "PR #42", &config_dir)
            .expect("render review prompt with a present template");

        assert_eq!(
            rendered,
            "Review of PR #42 on branch feature/diff-review (2 comments):\n\
             src/lib.rs:10 (new): fix this\n\nsrc/lib.rs:20 (old): remove dead code\n"
        );
    }

    #[test]
    fn render_review_prompt_returns_a_typed_error_when_the_template_is_missing() {
        // No `review.tmpl` written under this config dir at all (not even the
        // `prompts` directory) — this is the "operator never ran `pohunek
        // setup`" case DoD item 7 requires a typed error for, with no silent
        // default template.
        let config_dir = temp_config_dir("template-missing");
        let review = sample_review();

        let err = render_review_prompt_from_config_dir(&review, "PR #42", &config_dir)
            .expect_err("missing template must error, not silently render a default");

        match err {
            CoreError::MissingReviewTemplate { path } => {
                assert_eq!(path, config_dir.join("prompts").join("review.tmpl"));
            }
            other => panic!("expected CoreError::MissingReviewTemplate, got {other:?}"),
        }
    }
}
