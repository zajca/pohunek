use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const REFERENCE_GENERATED: &str = "generated";
const REFERENCE_MANUAL_ONLY: &str = "manual-only";

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("knowledge crate lives under crates/knowledge");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let out_bundle = out_dir.join("knowledge-bundle");

    let source = bundle_source(workspace_root);
    let reference_mode = reference_mode(&source);
    if out_bundle.exists() {
        fs::remove_dir_all(&out_bundle)?;
    }
    copy_dir_deterministic(&source, &source, &out_bundle)?;

    let content_hash = bundle_content_hash(&out_bundle)?;
    println!("cargo:rustc-env=POHUNEK_KNOWLEDGE_CONTENT_HASH={content_hash}");
    println!("cargo:rustc-env=POHUNEK_KNOWLEDGE_REFERENCE_MODE={reference_mode}");
    println!("cargo:rerun-if-env-changed=POHUNEK_KNOWLEDGE_BUNDLE");
    println!("cargo:rerun-if-changed={}", source.display());

    Ok(())
}

fn bundle_source(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = env::var("POHUNEK_KNOWLEDGE_BUNDLE") {
        return PathBuf::from(path);
    }

    workspace_root.join("docs").join("knowledge")
}

fn reference_mode(source: &Path) -> &'static str {
    if source.join("reference").is_dir() {
        REFERENCE_GENERATED
    } else {
        REFERENCE_MANUAL_ONLY
    }
}

fn copy_dir_deterministic(
    source_root: &Path,
    current: &Path,
    destination_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        entries.insert(entry.file_name(), entry.path());
    }

    for path in entries.values() {
        let relative = path.strip_prefix(source_root)?;
        let destination = destination_root.join(relative);
        let file_type = fs::symlink_metadata(path)?.file_type();
        if file_type.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_dir_deterministic(source_root, path, destination_root)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, destination)?;
        } else {
            return Err(format!("unsupported file type in {}", path.display()).into());
        }
    }

    Ok(())
}

fn bundle_content_hash(root: &Path) -> Result<String, Box<dyn Error>> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();

    let mut hasher = Sha256::new();
    for relative in files {
        let bytes = fs::read(root.join(&relative))?;
        let relative = relative_path_string(&relative)?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }

    let digest = hasher.finalize();
    let mut hex = String::with_capacity("sha256:".len() + digest.len() * 2);
    hex.push_str("sha256:");
    for byte in digest {
        write!(&mut hex, "{byte:02x}")?;
    }
    Ok(hex)
}

fn collect_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn relative_path_string(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut parts = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(format!("invalid relative bundle path {}", path.display()).into());
        };
        let Some(part) = part.to_str() else {
            return Err(format!("non-utf8 bundle path {}", path.display()).into());
        };
        parts.push(part);
    }
    Ok(parts.join("/"))
}
