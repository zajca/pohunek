//! Local prompt rendering commands.
//!
//! These commands are client-side only. They never connect to a daemon; scripts
//! use them to share the same renderer as the native GUI.

use std::fs;
use std::io::{self, Read as _};
use std::path::Path;

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
