//! Local prompt rendering commands.
//!
//! These commands are client-side only. They never connect to a daemon; scripts
//! use them to share the same renderer as the native GUI.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
use std::path::Path;

use pohunek_prompt::link::{SessionLinkKind, SessionLinkMetadata, SessionLinkProvider};
use pohunek_prompt::Provider;

use crate::error::CliError;

/// Render one provider prompt from a template file and provider JSON stdin.
///
/// The rendered prompt is written to stdout by the caller without adding bytes.
///
/// # Errors
///
/// Returns [`CliError`] when the template cannot be read, stdin cannot be read,
/// or the shared renderer rejects the provider JSON or template variables.
pub(crate) fn render_prompt(
    provider: Provider,
    item_id: &str,
    template_file: &Path,
) -> Result<String, CliError> {
    let template = fs::read_to_string(template_file)?;
    let mut context_json = String::new();
    io::stdin().read_to_string(&mut context_json)?;
    Ok(pohunek_prompt::render(
        template,
        provider,
        item_id,
        context_json,
    )?)
}

/// Builds canonical session link metadata from provider JSON.
///
/// The returned text is newline-delimited `key=value` output for shell callers.
///
/// # Errors
///
/// Returns [`CliError`] when stdin cannot be read, provider JSON is invalid,
/// the provider branch is missing, or shared link validation rejects a value.
pub(crate) fn link_metadata(
    provider: Provider,
    item_id: &str,
    url: &str,
) -> Result<String, CliError> {
    let mut context_json = String::new();
    io::stdin().read_to_string(&mut context_json)?;
    let branch = pohunek_prompt::link::branch_from_provider_json(provider, &context_json)?;
    let (link_provider, link_kind) = match provider {
        Provider::LinearIssue => (SessionLinkProvider::Linear, SessionLinkKind::Issue),
        Provider::GitHubPr => (SessionLinkProvider::GitHub, SessionLinkKind::PullRequest),
    };
    let metadata = SessionLinkMetadata::new(link_provider, link_kind, item_id, url, branch)?;

    let mut output = String::new();
    for (key, value) in metadata.to_session_metadata() {
        writeln!(&mut output, "{key}={value}").expect("writing to a string cannot fail");
    }
    Ok(output)
}
