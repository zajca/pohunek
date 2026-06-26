use std::fs;
use std::path::{Path, PathBuf};

use xtask::{build_docs, build_site, validate_docs, BuildOptions, SiteOptions};

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

#[test]
fn build_site_writes_matching_site_and_offline_outputs_with_relative_nav() {
    let temp = temp_dir("build-site");
    let source = temp.join("source");
    let target = temp.join("target");
    write_source_bundle(&source);

    let docs = build_docs(BuildOptions {
        source_dir: source,
        output_root: target.clone(),
        pohunek_version: "1.2.3-test".to_string(),
    })
    .expect("docs build succeeds");

    let site = build_site(SiteOptions {
        bundle_dir: docs.bundle_dir,
        manifest_path: docs.manifest_path.clone(),
        output_root: target,
    })
    .expect("site build succeeds");

    assert_eq!(site.pohunek_version, "1.2.3-test");
    assert!(site.site_dir.join("index.html").is_file());
    assert!(site.offline_dir.join("index.html").is_file());
    assert!(site.site_dir.join("concepts/example.html").is_file());
    assert!(site.offline_dir.join("concepts/example.html").is_file());

    let site_index = fs::read_to_string(site.site_dir.join("index.html")).expect("site index");
    let offline_index =
        fs::read_to_string(site.offline_dir.join("index.html")).expect("offline index");
    let site_page =
        fs::read_to_string(site.site_dir.join("concepts/example.html")).expect("site page");
    let offline_page =
        fs::read_to_string(site.offline_dir.join("concepts/example.html")).expect("offline page");

    assert_eq!(site_index, offline_index);
    assert_eq!(site_page, offline_page);
    assert!(offline_index.contains("pohunek 1.2.3-test"));
    assert!(offline_index.contains("href=\"concepts/example.html\""));
    assert!(offline_page.contains("href=\"../index.html\""));
    assert!(!offline_index.contains("href=\"/index.html\""));
    assert!(!offline_page.contains("href=\"/index.html\""));

    let manifest = fs::read_to_string(docs.manifest_path).expect("manifest exists");
    assert!(manifest.contains("\"pohunek_version\": \"1.2.3-test\""));
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
