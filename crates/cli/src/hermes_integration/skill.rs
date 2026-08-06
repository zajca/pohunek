//! Hermetic parity checks for the generated Pohunek skill asset.

// Rust guideline compliant 2026-08-06

#[cfg(test)]
mod tests {
    use std::path::Path;

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
    const REQUIRED_TOOLS: [&str; 7] = [
        "pohunek_hosts",
        "pohunek_sessions",
        "pohunek_session_get",
        "pohunek_session_screen",
        "pohunek_session_output",
        "pohunek_session_wait",
        "pohunek_session_diff",
    ];

    #[test]
    fn embedded_skill_frontmatter_and_tool_assets_have_exact_parity() {
        let assets = crate::hermes_integration::assets::render(Path::new("/tmp/policy.json"))
            .expect("render embedded plugin assets");
        let skill = asset_text(&assets, "skills/pohunek/SKILL.md");
        let manifest = asset_text(&assets, "plugin.yaml");
        let tools = asset_text(&assets, "tools.py");

        assert!(skill.starts_with("---\nname: pohunek\n"));
        assert_eq!(
            yaml_sequence(skill, "    requires_tools:", "      - "),
            REQUIRED_TOOLS
        );
        assert_eq!(
            yaml_sequence(manifest, "provides_tools:", "  - "),
            REGISTERED_TOOLS
        );
        assert_eq!(python_tool_schema_names(tools), REGISTERED_TOOLS);
        assert_eq!(REQUIRED_TOOLS, REGISTERED_TOOLS[..REQUIRED_TOOLS.len()]);
    }

    fn asset_text<'a>(
        assets: &'a [crate::hermes_integration::assets::Asset],
        path: &str,
    ) -> &'a str {
        let bytes = assets
            .iter()
            .find(|asset| asset.path() == path)
            .unwrap_or_else(|| panic!("missing embedded asset {path}"))
            .bytes();
        std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("embedded asset {path} is not UTF-8"))
    }

    fn yaml_sequence<'a>(document: &'a str, header: &str, item_prefix: &str) -> Vec<&'a str> {
        document
            .lines()
            .skip_while(|line| *line != header)
            .skip(1)
            .map_while(|line| line.strip_prefix(item_prefix))
            .collect()
    }

    fn python_tool_schema_names(source: &str) -> Vec<&str> {
        source
            .lines()
            .skip_while(|line| *line != "TOOL_SCHEMAS = {")
            .skip(1)
            .take_while(|line| *line != "}")
            .map(|line| {
                line.strip_prefix("    \"")
                    .and_then(|line| line.split_once("\": _schema("))
                    .map_or_else(
                        || panic!("non-canonical TOOL_SCHEMAS entry: {line}"),
                        |(name, _)| name,
                    )
            })
            .collect()
    }
}
