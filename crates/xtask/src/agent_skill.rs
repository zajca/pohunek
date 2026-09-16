//! Generates and checks the checked-in Pohunek agent-skill artifact.
//!
//! The generated artifact is embedded by the `pohunek agent-skill` command, so
//! keeping the render and parity check here makes source changes fail closed
//! until the checked artifact is regenerated.

// Rust guideline compliant 2026-09-15

use std::fs;
use std::path::{Path, PathBuf};

use crate::{create_dir_all, XtaskError};

pub(crate) const SOURCE_PATH: &str = "docs/knowledge/guides/agent-skill.md";
pub(crate) const GENERATED_PATH: &str = "crates/cli/src/commands/agent_skill/SKILL.md";
const SKILL_NAME: &str = "pohunek";
const SKILL_DESCRIPTION: &str = "Operate Pohunek safely from an agent: discover hosts and sessions, target exactly, read JSON state, subscribe to events, and defer destructive actions and approvals to the owner.";
const GENERATED_NOTICE: &str =
    "<!-- @generated: do not edit; run `cargo xtask agent-skill generate` -->";
const SOURCE_NOTICE: &str = "<!-- Source: docs/knowledge/guides/agent-skill.md -->";

/// Returns the repository-relative generated artifact path below `root`.
#[must_use]
pub(crate) fn generated_path(root: &Path) -> PathBuf {
    root.join(GENERATED_PATH)
}

/// Writes the deterministic skill artifact rendered from the knowledge source.
pub(crate) fn generate(root: &Path) -> Result<(), XtaskError> {
    let path = generated_path(root);
    let bytes = render(root)?;
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    fs::write(&path, bytes).map_err(|source| XtaskError::Io { path, source })
}

/// Checks that the committed skill artifact is present and exactly current.
pub(crate) fn check(root: &Path) -> Result<bool, XtaskError> {
    let expected = render(root)?;
    let path = generated_path(root);
    match fs::read(&path) {
        Ok(actual) if actual == expected => {
            println!("[PASS] agent-skill: generated artifact is current");
            Ok(true)
        }
        Ok(_) => {
            println!(
                "[FAIL] agent-skill: {GENERATED_PATH} is stale; run `cargo xtask agent-skill generate`"
            );
            Ok(false)
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "[FAIL] agent-skill: {GENERATED_PATH} is missing; run `cargo xtask agent-skill generate`"
            );
            Ok(false)
        }
        Err(source) => Err(XtaskError::Io { path, source }),
    }
}

/// Returns the current checked artifact when it exists for content validation.
pub(crate) fn read_checked(root: &Path) -> Result<Option<Vec<u8>>, XtaskError> {
    let path = generated_path(root);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(XtaskError::Io { path, source }),
    }
}

fn render(root: &Path) -> Result<Vec<u8>, XtaskError> {
    let source_path = root.join(SOURCE_PATH);
    let source = read_utf8(&source_path)?;
    let body = strip_frontmatter(&source)?;

    let mut output = String::from("---\n");
    output.push_str("name: ");
    output.push_str(SKILL_NAME);
    output.push('\n');
    output.push_str("description: ");
    output.push_str(SKILL_DESCRIPTION);
    output.push_str("\n---\n\n");
    output.push_str(GENERATED_NOTICE);
    output.push('\n');
    output.push_str(SOURCE_NOTICE);
    output.push_str("\n\n");
    output.push_str(body);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output.into_bytes())
}

fn read_utf8(path: &Path) -> Result<String, XtaskError> {
    fs::read_to_string(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn strip_frontmatter(source: &str) -> Result<&str, XtaskError> {
    let source = source.strip_prefix("---\n").ok_or_else(|| {
        XtaskError::Usage(format!("{SOURCE_PATH} must begin with YAML frontmatter"))
    })?;
    let end = source.find("\n---\n").ok_or_else(|| {
        XtaskError::Usage(format!(
            "{SOURCE_PATH} must terminate YAML frontmatter with `---`"
        ))
    })?;
    let body = &source[end + "\n---\n".len()..];
    if body.trim().is_empty() {
        return Err(XtaskError::Usage(format!(
            "{SOURCE_PATH} must contain a non-empty Markdown body"
        )));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Section headings the skill content must cover: the mission boundary,
    /// the eight issue coverage areas, and the explicit safety boundaries.
    const REQUIRED_SECTIONS: [&str; 10] = [
        "## Mission and trust boundary",
        "## Discovery",
        "## Safe targeting",
        "## Reading state",
        "## Subscribing to events",
        "## Sending prompts and waiting",
        "## Diffs and worktrees",
        "## Destructive operations",
        "## Blocked agents and approvals",
        "## Explicit safety boundaries",
    ];

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "pohunek-agent-skill-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create temporary root");
        TempDir(path)
    }

    fn write_source(root: &Path, source: &str) {
        let source_path = root.join(SOURCE_PATH);
        fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
        fs::write(source_path, source).expect("write source");
    }

    fn source() -> &'static str {
        "---\ntype: Guide\n---\n\n# Pohunek agent skill\n\nUse `pohunek doctor --json`.\n"
    }

    #[test]
    fn deterministic_renderer_strips_knowledge_frontmatter() {
        let root = temp_root();
        write_source(&root.0, source());

        let first = render(&root.0).expect("render first skill");
        assert_eq!(first, render(&root.0).expect("render second skill"));
        let skill = String::from_utf8(first).expect("valid UTF-8");
        assert!(skill.starts_with("---\nname: pohunek\n"));
        assert!(skill.contains(GENERATED_NOTICE));
        assert!(skill.contains(SOURCE_NOTICE));
        assert!(skill.ends_with("# Pohunek agent skill\n\nUse `pohunek doctor --json`.\n"));
        assert!(!skill.contains("type: Guide"));
    }

    #[test]
    fn checker_detects_missing_stale_and_changed_source() {
        let root = temp_root();
        write_source(&root.0, source());
        assert!(!check(&root.0).expect("check missing skill"));

        generate(&root.0).expect("generate skill");
        assert!(check(&root.0).expect("check generated skill"));

        fs::write(generated_path(&root.0), b"stale\n").expect("write stale skill");
        assert!(!check(&root.0).expect("check stale skill"));

        generate(&root.0).expect("regenerate skill");
        let source_path = root.0.join(SOURCE_PATH);
        fs::write(source_path, source().replace("Use", "Safely use")).expect("change source");
        assert!(!check(&root.0).expect("check changed source"));
    }

    #[test]
    fn renderer_requires_frontmatter_and_nonempty_body() {
        let root = temp_root();
        write_source(&root.0, "# no frontmatter\n");
        render(&root.0).expect_err("missing frontmatter must fail");

        write_source(&root.0, "---\ntype: Guide\n");
        render(&root.0).expect_err("unterminated frontmatter must fail");

        write_source(&root.0, "---\ntype: Guide\n---\n\n   \n");
        render(&root.0).expect_err("empty body must fail");
    }

    #[test]
    fn repo_source_covers_required_sections_and_single_trailing_newline() {
        let root = crate::repo_root();
        let rendered = String::from_utf8(render(&root).expect("render repository skill"))
            .expect("rendered skill is UTF-8");

        for section in REQUIRED_SECTIONS {
            assert!(
                rendered.contains(section),
                "skill source is missing required section heading: {section}"
            );
        }
        assert!(
            rendered.ends_with('\n') && !rendered.ends_with("\n\n"),
            "generated skill must end with exactly one trailing newline"
        );
    }

    #[test]
    fn repo_generated_artifact_matches_the_current_render() {
        let root = crate::repo_root();
        let checked = read_checked(&root)
            .expect("read checked artifact")
            .expect("checked-in agent-skill artifact must exist");
        assert_eq!(
            checked,
            render(&root).expect("render repository skill"),
            "checked-in artifact is stale; run `cargo xtask agent-skill generate`"
        );
    }
}
