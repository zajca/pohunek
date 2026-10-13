//! Generates and checks the embedded Hermes Pohunek skill payload.
//!
//! The generated artifact has one hand-authored body source and an explicit,
//! fixed plugin tool contract. Keeping both checks here makes source changes
//! fail closed until the checked asset is regenerated.

// Rust guideline compliant 2026-08-06

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use toml_edit::{value, DocumentMut};

use crate::{create_dir_all, XtaskError};

pub(crate) const SOURCE_PATH: &str = "docs/knowledge/guides/hermes-operator.md";
pub(crate) const GENERATED_PATH: &str = "runtime-packages/hermes/operator/skills/pohunek/SKILL.md";
const PLUGIN_PATH: &str = "runtime-packages/hermes/operator/plugin.yaml";
const SIDECAR_PATH: &str = "runtime-packages/hermes/operator.toml";
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
    fs::write(&path, bytes)
        .map_err(|source| XtaskError::Io { path, source })
        .and_then(|()| update_sidecar_checksum(root))
}

/// Checks that the committed skill artifact is present and exactly current.
pub(crate) fn check(root: &Path) -> Result<bool, XtaskError> {
    let expected = render(root)?;
    let path = generated_path(root);
    match fs::read(&path) {
        Ok(actual) if actual == expected => check_sidecar_checksum(root, &expected),
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

fn check_sidecar_checksum(root: &Path, skill: &[u8]) -> Result<bool, XtaskError> {
    let path = root.join(SIDECAR_PATH);
    let source = read_utf8(&path)?;
    let document = source
        .parse::<DocumentMut>()
        .map_err(|error| XtaskError::Usage(format!("{SIDECAR_PATH} is invalid TOML: {error}")))?;
    let expected = checksum(skill);
    let recorded = sidecar_skill_record(&document)
        .and_then(|record| record.get("sha256"))
        .and_then(toml_edit::Item::as_str);
    if recorded == Some(expected.as_str()) {
        println!("[PASS] hermes-skill: embedded operator payload and sidecar checksum are current");
        Ok(true)
    } else {
        println!(
            "[FAIL] hermes-skill: {SIDECAR_PATH} has a stale skill checksum; run `cargo xtask hermes generate-skill`"
        );
        Ok(false)
    }
}

fn update_sidecar_checksum(root: &Path) -> Result<(), XtaskError> {
    let manifest_path = root.join(SIDECAR_PATH);
    let mut document = read_utf8(&manifest_path)?
        .parse::<DocumentMut>()
        .map_err(|error| XtaskError::Usage(format!("{SIDECAR_PATH} is invalid TOML: {error}")))?;
    let skill_path = root.join(GENERATED_PATH);
    let skill = fs::read(&skill_path).map_err(|source| XtaskError::Io {
        path: skill_path,
        source,
    })?;
    let record = sidecar_skill_record_mut(&mut document).ok_or_else(|| {
        XtaskError::Usage(format!(
            "{SIDECAR_PATH} must contain exactly one payload record for {GENERATED_PATH}"
        ))
    })?;
    record["sha256"] = value(checksum(&skill));
    fs::write(&manifest_path, document.to_string()).map_err(|source| XtaskError::Io {
        path: manifest_path,
        source,
    })
}

fn sidecar_skill_record(document: &DocumentMut) -> Option<&toml_edit::Table> {
    let records = document.get("payload")?.as_array_of_tables()?;
    let mut matches = records.iter().filter(|record| {
        record.get("path").and_then(toml_edit::Item::as_str) == Some("skills/pohunek/SKILL.md")
    });
    let record = matches.next()?;
    matches.next().is_none().then_some(record)
}

fn sidecar_skill_record_mut(document: &mut DocumentMut) -> Option<&mut toml_edit::Table> {
    let records = document.get_mut("payload")?.as_array_of_tables_mut()?;
    let matches = records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            record.get("path").and_then(toml_edit::Item::as_str) == Some("skills/pohunek/SKILL.md")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return None;
    }
    records.get_mut(matches[0])
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

    use pohunek_test_support::env::TestEnv;
    use pohunek_test_support::process_env::ProcessEnv;

    use super::*;

    /// A private fixture root that is removed when the guard drops.
    fn temp_root() -> tempfile::TempDir {
        pohunek_test_support::tempdir_with_prefix("pxs-").expect("private fixture root")
    }

    fn write_sources(root: &Path, source: &str, plugin: &str) {
        let source_path = root.join(SOURCE_PATH);
        let plugin_path = root.join(PLUGIN_PATH);
        let sidecar_path = root.join(SIDECAR_PATH);
        fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
        fs::create_dir_all(plugin_path.parent().expect("plugin parent")).expect("plugin parent");
        fs::create_dir_all(sidecar_path.parent().expect("sidecar parent")).expect("sidecar parent");
        fs::write(source_path, source).expect("write source");
        fs::write(plugin_path, plugin).expect("write plugin");
        fs::write(
            sidecar_path,
            "schema = 1\n\n[[payload]]\npath = \"skills/pohunek/SKILL.md\"\nsha256 = \"stale\"\n",
        )
        .expect("write operator sidecar");
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
    fn public_commands_generate_and_check_the_embedded_operator_payload() {
        const FIXTURE_ROOT_VAR: &str = "POHUNEK_HERMES_SKILL_FIXTURE_ROOT";

        let _process_env = ProcessEnv::lock();
        if let Some(root) = std::env::var_os(FIXTURE_ROOT_VAR) {
            exercise_public_commands(Path::new(&root));
            return;
        }

        let env = TestEnv::new().expect("hermetic fixture environment");
        let root = env.cwd();
        let manifest_dir = root.join("crates/xtask");
        fs::create_dir_all(&manifest_dir).expect("fixture xtask package directory");
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n")
            .expect("fixture workspace manifest");
        let guide_path = root.join("docs/knowledge/guides/hermes-operator.md");
        fs::create_dir_all(guide_path.parent().expect("guide parent"))
            .expect("fixture guide parent");
        fs::write(&guide_path, source()).expect("write fixture guide");
        let plugin_path = root.join("runtime-packages/hermes/operator/plugin.yaml");
        fs::create_dir_all(plugin_path.parent().expect("plugin parent"))
            .expect("fixture plugin parent");
        fs::write(&plugin_path, plugin()).expect("write fixture plugin");
        let sidecar_path = root.join("runtime-packages/hermes/operator.toml");
        fs::write(
            &sidecar_path,
            "schema = 1\n\n[[payload]]\npath = \"skills/pohunek/SKILL.md\"\nsha256 = \"stale\"\n",
        )
        .expect("write fixture sidecar");

        let executable = std::env::current_exe().expect("test executable path");
        let output = env
            .command(executable)
            .args([
                "--exact",
                "hermes_skill::tests::public_commands_generate_and_check_the_embedded_operator_payload",
            ])
            .env(FIXTURE_ROOT_VAR, root)
            .env("CARGO_MANIFEST_DIR", &manifest_dir)
            .output()
            .expect("run isolated public-command test child");
        assert!(
            output.status.success(),
            "isolated public-command test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn exercise_public_commands(root: &Path) {
        let guide_path = root.join("docs/knowledge/guides/hermes-operator.md");
        let changed_source = source().replace("Use", "Safely use");
        fs::write(&guide_path, &changed_source).expect("change source guide");
        crate::run(["hermes", "generate-skill"]).expect("run public generator command");

        let generated_path = root.join("runtime-packages/hermes/operator/skills/pohunek/SKILL.md");
        let generated = fs::read(&generated_path).expect("read embedded payload target");
        let generated_text = String::from_utf8(generated.clone()).expect("generated UTF-8");
        assert!(generated_text.ends_with("# Hermes operator\n\nSafely use `pohunek_sessions`.\n"));
        let sidecar_path = root.join("runtime-packages/hermes/operator.toml");
        let sidecar = fs::read_to_string(&sidecar_path).expect("read fixture sidecar");
        let expected_digest = format!("{:x}", Sha256::digest(&generated));
        assert!(sidecar.contains(&format!("sha256 = \"{expected_digest}\"")));
        crate::run(["hermes", "check-skill"]).expect("check generated payload");

        fs::write(&sidecar_path, sidecar.replace(&expected_digest, "corrupt"))
            .expect("corrupt sidecar digest");
        assert!(crate::run(["hermes", "check-skill"]).is_err());
        crate::run(["hermes", "generate-skill"]).expect("repair checksum through CLI");

        fs::write(&generated_path, b"corrupt body\n").expect("corrupt embedded payload");
        assert!(crate::run(["hermes", "check-skill"]).is_err());
        crate::run(["hermes", "generate-skill"]).expect("repair payload through CLI");
        crate::run(["hermes", "check-skill"]).expect("check regenerated payload");
    }

    #[test]
    fn deterministic_renderer_strips_knowledge_frontmatter() {
        let root = temp_root();
        write_sources(root.path(), source(), &plugin());

        let first = render(root.path()).expect("render first skill");
        assert_eq!(first, render(root.path()).expect("render second skill"));
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
        write_sources(root.path(), source(), &plugin());
        assert!(!check(root.path()).expect("check missing skill"));

        generate(root.path()).expect("generate skill");
        assert!(check(root.path()).expect("check generated skill"));

        fs::write(generated_path(root.path()), b"stale\n").expect("write stale skill");
        assert!(!check(root.path()).expect("check stale skill"));

        generate(root.path()).expect("regenerate skill");
        let source_path = root.path().join(SOURCE_PATH);
        fs::write(source_path, source().replace("Use", "Safely use")).expect("change source");
        assert!(!check(root.path()).expect("check changed source"));
    }

    #[test]
    fn validator_requires_exact_seven_and_sixteen_tool_contract() {
        let root = temp_root();
        let missing = tool_lines(&REGISTERED_TOOLS[..15]);
        let plugin = format!("name: pohunek\nprovides_tools:\n{missing}provides_hooks:\n");
        write_sources(root.path(), source(), &plugin);
        render(root.path()).expect_err("missing registered tool must fail");

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
        write_sources(root.path(), source(), &plugin);
        render(root.path()).expect_err("reordered registered tools must fail");
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
