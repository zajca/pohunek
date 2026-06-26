use std::fs;
use std::path::{Path, PathBuf};

use xtask::{build_docs, validate_docs, BuildOptions};

#[test]
fn validate_docs_reports_committed_bundle() {
    let report = validate_docs(repo_root().join("docs/knowledge")).expect("bundle validates");

    assert!(report.files_checked > 0);
    assert!(report.concept_count > 0);
}

#[test]
fn build_docs_copies_files_and_writes_deterministic_manifest() {
    let temp = temp_dir("build-docs");
    let source = temp.join("source");
    let target = temp.join("target");
    write_source_bundle(&source);

    let first = build_docs(BuildOptions {
        source_dir: source.clone(),
        output_root: target.clone(),
        pohunek_version: "0.0.0-test".to_string(),
    })
    .expect("first build succeeds");
    let first_manifest = fs::read_to_string(first.manifest_path).expect("manifest exists");

    let second = build_docs(BuildOptions {
        source_dir: source,
        output_root: target,
        pohunek_version: "0.0.0-test".to_string(),
    })
    .expect("second build succeeds");
    let second_manifest = fs::read_to_string(second.manifest_path).expect("manifest exists");

    assert_eq!(first_manifest, second_manifest);
    assert_eq!(first.content_hash, second.content_hash);
    assert!(second.content_hash.starts_with("sha256:"));
    assert_eq!(second.content_hash.len(), "sha256:".len() + 64);
    // The generated reference concepts are always included, so the total file
    // count exceeds the 2 manual source files.
    assert!(
        first.files_copied >= 2,
        "at least the manual source files are copied"
    );
    assert!(second.bundle_dir.join("index.md").is_file());
    assert!(second.bundle_dir.join("concepts/example.md").is_file());
    // Generated reference directories are always present.
    assert!(second.bundle_dir.join("reference/cli").is_dir());
    assert!(second.bundle_dir.join("reference/protocol").is_dir());
    assert!(second.bundle_dir.join("reference/config").is_dir());
    assert!(second.bundle_dir.join("reference/setup-assets").is_dir());

    let manifest: serde_json::Value =
        serde_json::from_str(&second_manifest).expect("manifest is json");
    assert_eq!(manifest["pohunek_version"], "0.0.0-test");
    assert_eq!(manifest["knowledge_schema_version"], 1);
    assert_eq!(manifest["reference"], "generated");
    let sources = manifest["sources"].as_array().expect("sources is array");
    assert!(sources.contains(&serde_json::json!("manual_docs")));
    assert!(sources.contains(&serde_json::json!("cli")));
    assert!(sources.contains(&serde_json::json!("protocol")));
    assert_eq!(manifest["content_hash"], second.content_hash);
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask crate lives under crates/xtask")
        .to_path_buf()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pohunek-xtask-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_source_bundle(root: &Path) {
    fs::create_dir_all(root.join("concepts")).expect("create concepts dir");
    fs::write(root.join("index.md"), "# Index\n").expect("write index");
    fs::write(
        root.join("concepts/example.md"),
        concat!(
            "---\n",
            "type: Concept\n",
            "id: example\n",
            "title: Example\n",
            "description: Example concept.\n",
            "source_kind: manual\n",
            "---\n",
            "\n",
            "# Example\n"
        ),
    )
    .expect("write concept");
}
