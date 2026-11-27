// Links the prebuilt libghostty-vt static library when the `libghostty`
// feature is on. It never downloads anything and never runs Zig: the library
// comes from `scripts/build-libghostty-vt`, and LIBGHOSTTY_VT_DIR names its
// output directory.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const BUILD_SCRIPT: &str = "scripts/build-libghostty-vt";

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
}

/// Parses `key=value` lines of `BUILD_INFO` and `abi-layout.txt`; `#` lines are comments.
fn key_values(path: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", path.display())));
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// The pinned library version, the `version` key of the `[library]` table of the pin file.
fn pinned_version(manifest_dir: &Path) -> String {
    let path = manifest_dir.join("../../scripts/libghostty-vt.pin.toml");
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", path.display())));
    let mut in_library = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_library = line == "[library]";
        } else if in_library {
            if let Some(version) = line
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
            {
                return version.to_owned();
            }
        }
    }
    fail("scripts/libghostty-vt.pin.toml has no [library] version")
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_DIR");
    if env::var_os("CARGO_FEATURE_LIBGHOSTTY").is_none() {
        return;
    }
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let Some(dir) = env::var_os("LIBGHOSTTY_VT_DIR") else {
        fail(&format!(
            "the `libghostty` feature needs LIBGHOSTTY_VT_DIR, the output directory of {BUILD_SCRIPT}"
        ));
    };
    let dir = PathBuf::from(dir);
    let info_path = dir.join("BUILD_INFO");
    println!("cargo:rerun-if-changed={}", info_path.display());
    let info = key_values(&info_path);
    let pinned = pinned_version(&manifest_dir);
    let target = env::var("TARGET").expect("cargo sets it");
    if info.get("library_version") != Some(&pinned) {
        fail(&format!(
            "{} holds library {:?} but the pin is {pinned}; rebuild with {BUILD_SCRIPT}",
            info_path.display(),
            info.get("library_version")
        ));
    }
    if info.get("rust_target") != Some(&target) {
        fail(&format!(
            "{} was built for target {:?} but cargo builds for {target}; rebuild with {BUILD_SCRIPT}",
            info_path.display(),
            info.get("rust_target")
        ));
    }

    let layout_path = dir.join("abi-layout.txt");
    println!("cargo:rerun-if-changed={}", layout_path.display());
    let layout = key_values(&layout_path);
    if layout.is_empty() {
        fail(&format!(
            "{} holds no layout: the probe could not run on the build host; build on a host that runs {target}",
            layout_path.display()
        ));
    }
    let mut constants = String::new();
    for (name, value) in &layout {
        if !name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            || value.parse::<usize>().is_err()
        {
            fail(&format!(
                "{}: unexpected line {name}={value}",
                layout_path.display()
            ));
        }
        writeln!(
            constants,
            "#[allow(dead_code, reason = \"the probe records more layout than the mirrors assert\")]\n\
             pub(crate) const {name}: usize = {value};"
        )
        .expect("write to a string");
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets it")).join("abi_layout.rs");
    fs::write(&out, constants).unwrap_or_else(|error| fail(&format!("{}: {error}", out.display())));

    println!(
        "cargo:rustc-link-search=native={}",
        dir.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}
