//! Generates and checks the checked-in Hermes Pohunek skill asset.
//!
//! The generated artifact has one hand-authored body source and an explicit,
//! fixed plugin tool contract. Keeping both checks here makes source changes
//! fail closed until the checked asset is regenerated.

// Rust guideline compliant 2026-08-06

use std::fs;
use std::path::{Path, PathBuf};

use crate::{create_dir_all, XtaskError};

pub(crate) const SOURCE_PATH: &str = "docs/knowledge/guides/hermes-operator.md";
pub(crate) const GENERATED_PATH: &str =
    "crates/cli/src/hermes_integration/assets/pohunek/skills/pohunek/SKILL.md";
const PLUGIN_PATH: &str = "crates/cli/src/hermes_integration/assets/pohunek/plugin.yaml";
const SKILL_NAME: &str = "pohunek";
const SKILL_DESCRIPTION: &str =
    "Safely observe and operate Pohunek sessions through registered tools.";
const GENERATED_NOTICE: &str =
    "<!-- @generated: do not edit; run `cargo xtask hermes generate-skill` -->";
const SOURCE_NOTICE: &str = "<!-- Source: docs/knowledge/guides/hermes-operator.md -->";
const REQUIRED_TOOLS: [&str; 7] = [
    "pohunek_hosts",
    "pohunek_sessions",
    "pohunek_session_get",
    "pohunek_session_screen",
    "pohunek_session_output",
    "pohunek_session_wait",
    "pohunek_session_diff",
];
const REGISTERED_TOOLS: [&str; 16] = [
    "pohunek_hosts",
    "pohunek_sessions",
    "pohunek_session_get",
    "pohunek_session_screen",
    "pohunek_session_output",
    "pohunek_session_wait",
    "pohunek_session_diff",
    "pohunek_session_start",
    "pohunek_session_send",
    "pohunek_session_resume",
    "pohunek_session_fork",
    "pohunek_session_resize",
    "pohunek_session_rename",
    "pohunek_session_set_metadata",
    "pohunek_session_stop",
    "pohunek_session_remove",
];

/// Returns the repository-relative generated asset path below `root`.
#[must_use]
pub(crate) fn generated_path(root: &Path) -> PathBuf {
    root.join(GENERATED_PATH)
}

/// Writes the deterministic skill artifact after validating the plugin surface.
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
            println!("[PASS] hermes-skill: generated asset is current");
            Ok(true)
        }
        Ok(_) => {
            println!(
                "[FAIL] hermes-skill: {GENERATED_PATH} is stale; run `cargo xtask hermes generate-skill`"
            );
            Ok(false)
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "[FAIL] hermes-skill: {GENERATED_PATH} is missing; run `cargo xtask hermes generate-skill`"
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
    let plugin_path = root.join(PLUGIN_PATH);
    let source = read_utf8(&source_path)?;
    let plugin = read_utf8(&plugin_path)?;
    validate_plugin_tools(&plugin)?;
    let body = strip_frontmatter(&source)?;

    let mut output = String::from("---\n");
    output.push_str("name: ");
    output.push_str(SKILL_NAME);
    output.push('\n');
    output.push_str("description: ");
    output.push_str(SKILL_DESCRIPTION);
    output.push_str("\nmetadata:\n  hermes:\n    requires_tools:\n");
    for tool in REQUIRED_TOOLS {
        output.push_str("      - ");
        output.push_str(tool);
        output.push('\n');
    }
    output.push_str("---\n\n");
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

fn validate_plugin_tools(plugin: &str) -> Result<(), XtaskError> {
    let tools = parse_plugin_tools(plugin)?;
    let expected: Vec<String> = REGISTERED_TOOLS.iter().map(ToString::to_string).collect();
    if tools != expected {
        return Err(XtaskError::Usage(format!(
            "{PLUGIN_PATH} provides_tools must exactly match the 16 registered Hermes tools"
        )));
    }
    if tools[..REQUIRED_TOOLS.len()] != REQUIRED_TOOLS.map(ToString::to_string) {
        return Err(XtaskError::Usage(format!(
            "{PLUGIN_PATH} first seven provides_tools entries must exactly match the required read tools"
        )));
    }
    Ok(())
}

fn parse_plugin_tools(plugin: &str) -> Result<Vec<String>, XtaskError> {
    let mut tool_section = None;
    for (index, line) in plugin.lines().enumerate() {
        if line == "provides_tools:" && tool_section.replace(index).is_some() {
            return Err(XtaskError::Usage(format!(
                "{PLUGIN_PATH} must declare exactly one top-level provides_tools section"
            )));
        }
    }
    let start = tool_section.ok_or_else(|| {
        XtaskError::Usage(format!(
            "{PLUGIN_PATH} must declare top-level provides_tools"
        ))
    })?;
    let mut tools = Vec::new();
    for line in plugin.lines().skip(start + 1) {
        if is_top_level_key(line) {
            break;
        }
        let tool = line.strip_prefix("  - ").ok_or_else(|| {
            XtaskError::Usage(format!(
                "{PLUGIN_PATH} provides_tools must be one canonical uninterrupted YAML sequence"
            ))
        })?;
        if !is_tool_name(tool) || tools.iter().any(|known| known == tool) {
            return Err(XtaskError::Usage(format!(
                "{PLUGIN_PATH} contains an invalid or duplicate provides_tools entry"
            )));
        }
        tools.push(tool.to_owned());
    }
    if tools.is_empty() {
        return Err(XtaskError::Usage(format!(
            "{PLUGIN_PATH} provides_tools must contain a non-empty exact sequence"
        )));
    }
    Ok(tools)
}

fn is_top_level_key(line: &str) -> bool {
    let Some((key, _value)) = line.split_once(':') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_tool_name(value: &str) -> bool {
    value.starts_with("pohunek_")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "pohunek-hermes-skill-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create temporary root");
        TempDir(path)
    }

    fn write_sources(root: &Path, source: &str, plugin: &str) {
        let source_path = root.join(SOURCE_PATH);
        let plugin_path = root.join(PLUGIN_PATH);
        fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
        fs::create_dir_all(plugin_path.parent().expect("plugin parent")).expect("plugin parent");
        fs::write(source_path, source).expect("write source");
        fs::write(plugin_path, plugin).expect("write plugin");
    }

    fn source() -> &'static str {
        "---\ntype: Guide\n---\n\n# Hermes operator\n\nUse `pohunek_sessions`.\n"
    }

    fn tool_lines(tools: &[&str]) -> String {
        let mut lines = String::new();
        for tool in tools {
            writeln!(lines, "  - {tool}").expect("write tool fixture line");
        }
        lines
    }

    fn plugin() -> String {
        let tools = tool_lines(&REGISTERED_TOOLS);
        format!("name: pohunek\nprovides_tools:\n{tools}provides_hooks:\n  - on_session_start\n")
    }

    #[test]
    fn deterministic_renderer_strips_knowledge_frontmatter() {
        let root = temp_root();
        write_sources(&root.0, source(), &plugin());

        let first = render(&root.0).expect("render first skill");
        assert_eq!(first, render(&root.0).expect("render second skill"));
        let skill = String::from_utf8(first).expect("valid UTF-8");
        assert!(skill.starts_with("---\nname: pohunek\n"));
        assert!(skill.contains(GENERATED_NOTICE));
        assert!(skill.contains(SOURCE_NOTICE));
        assert!(skill.ends_with("# Hermes operator\n\nUse `pohunek_sessions`.\n"));
        assert!(!skill.contains("type: Guide"));
    }

    #[test]
    fn checker_detects_missing_stale_and_changed_source() {
        let root = temp_root();
        write_sources(&root.0, source(), &plugin());
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
    fn validator_requires_exact_seven_and_sixteen_tool_contract() {
        let root = temp_root();
        let missing = tool_lines(&REGISTERED_TOOLS[..15]);
        let plugin = format!("name: pohunek\nprovides_tools:\n{missing}provides_hooks:\n");
        write_sources(&root.0, source(), &plugin);
        render(&root.0).expect_err("missing registered tool must fail");

        let reordered = tool_lines(&[
            REGISTERED_TOOLS[1],
            REGISTERED_TOOLS[0],
            REGISTERED_TOOLS[2],
            REGISTERED_TOOLS[3],
            REGISTERED_TOOLS[4],
            REGISTERED_TOOLS[5],
            REGISTERED_TOOLS[6],
            REGISTERED_TOOLS[7],
            REGISTERED_TOOLS[8],
            REGISTERED_TOOLS[9],
            REGISTERED_TOOLS[10],
            REGISTERED_TOOLS[11],
            REGISTERED_TOOLS[12],
            REGISTERED_TOOLS[13],
            REGISTERED_TOOLS[14],
            REGISTERED_TOOLS[15],
        ]);
        let plugin = format!("name: pohunek\nprovides_tools:\n{reordered}provides_hooks:\n");
        write_sources(&root.0, source(), &plugin);
        render(&root.0).expect_err("reordered registered tools must fail");
    }

    #[test]
    fn parser_rejects_interrupted_and_duplicate_tool_sections() {
        let extra = "  - pohunek_unexpected_extra\n";
        for interruption in ["  # hidden continuation\n", "\n"] {
            let malformed = plugin().replacen(
                "provides_hooks:",
                &format!("{interruption}{extra}provides_hooks:"),
                1,
            );
            assert!(
                parse_plugin_tools(&malformed).is_err(),
                "accepted interruption {interruption:?} before an extra item"
            );
        }

        let duplicate = format!("{}provides_tools:\n  - pohunek_hosts\n", plugin());
        parse_plugin_tools(&duplicate).expect_err("duplicate tool sections must fail");
    }
}
