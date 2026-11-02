//! Binary-level scenarios of the macOS admin-group exception of the
//! executable-lookup trust policy.
//!
//! Each scenario runs the real `pohunek` binary with `providers =
//! ["tailscale"]` against a fake `tailscale` placed in a directory whose
//! owner, group and mode vary. The runner user must be a member of the `admin`
//! group (gid 80) and of `staff` (gid 20); the scenarios fail, never skip,
//! when it is not. The fake serves a spec-derived status document (see
//! `support/tailscale_status.rs`).

#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-10-23

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use serde_json::Value;

#[path = "support/tailscale_status.rs"]
mod tailscale_status;

use tailscale_status::{FakeTailscale, Node, Status};

/// The macOS `admin` group.
const ADMIN_GID: u32 = 80;

/// The macOS `staff` group, a group other than `admin`.
const STAFF_GID: u32 = 20;

/// This host's address in the fixtures.
const SELF_ADDRESS: &str = "100.64.0.7";

/// A `PATH` that holds the system tools the fake needs and nothing else.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

struct Fixture {
    env: TestEnv,
    fake: FakeTailscale,
}

impl Fixture {
    fn new() -> Self {
        assert_groups();
        let env = TestEnv::new().expect("create the hermetic test environment");
        let scratch = env.root().join("scratch");
        fs::create_dir_all(&scratch).expect("create the scratch directory");
        let fake = FakeTailscale::new(&scratch);
        fake.install(&Status::running(
            SELF_ADDRESS,
            vec![Node::peer(
                "alpha",
                "nALPHA00001CNTRL",
                &["100.64.0.10", "fd7a:115c:a1e0::10"],
            )],
        ));
        let dir = env.config_home().join("pohunek");
        fs::create_dir_all(&dir).expect("create the config directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .expect("make the config directory private");
        let file = dir.join("overlays.toml");
        fs::write(&file, "providers = [\"tailscale\"]\nport = 9100\n")
            .expect("write overlays.toml");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .expect("make overlays.toml private");
        Self { env, fake }
    }

    /// The scratch fake, moved to `directory/tailscale` with `mode`.
    fn place_fake(&self, directory: &Path, mode: u32) -> PathBuf {
        let target = directory.join("tailscale");
        fs::rename(self.fake.bin_dir().join("tailscale"), &target).expect("move the fake");
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).expect("set the fake mode");
        target
    }

    /// A directory under a private root with the given group and mode.
    fn directory(&self, name: &str, gid: u32, mode: u32) -> PathBuf {
        let path = self.env.root().join(name);
        fs::create_dir_all(&path).expect("create the directory");
        set_group_and_mode(&path, gid, mode);
        path
    }

    fn run(&self, path: &str, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .env("PATH", path)
            .args(args)
            .output()
            .expect("run the pohunek CLI")
    }

    fn host_list(&self, path: &str) -> Output {
        self.run(path, &["host", "list", "--refresh", "--json"])
    }

    fn doctor_cli_detail(&self, path: &str) -> String {
        let output = self.run(path, &["doctor", "--json"]);
        let document = json(&output);
        let checks = document["ok"]["checks"].as_array().expect("doctor checks");
        let check = checks
            .iter()
            .find(|check| check["name"] == "tailscale_cli")
            .unwrap_or_else(|| panic!("tailscale_cli in {checks:?}"));
        check["detail"].as_str().expect("detail").to_owned()
    }
}

/// Fails hard unless the runner belongs to `admin` and `staff`.
fn assert_groups() {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-G")
        .output()
        .expect("run id -G");
    let groups: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|group| group.parse().ok())
        .collect();
    for required in [ADMIN_GID, STAFF_GID] {
        assert!(
            groups.contains(&required),
            "the runner must be in group {required}, it is in {groups:?}"
        );
    }
}

fn set_group_and_mode(path: &Path, gid: u32, mode: u32) {
    std::os::unix::fs::chown(path, None, Some(gid)).expect("set the group");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set the mode");
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is one JSON document ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_resolved(fixture: &Fixture, path: &str) {
    let output = fixture.host_list(path);
    assert!(
        output.status.success(),
        "the listing failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let records = json(&output)["ok"]
        .as_array()
        .expect("host records")
        .clone();
    assert!(
        records.iter().any(|record| record["name"] == "alpha"),
        "the fake's peer is listed: {records:?}"
    );
    assert!(
        fixture.fake.invocations() >= 1,
        "the fake served the listing"
    );
    let detail = fixture.doctor_cli_detail(path);
    assert!(
        !detail.contains("not installed") && !detail.contains("unusable"),
        "the doctor accepts the CLI: {detail}"
    );
}

/// The refused fake never runs and its peer is never listed.
///
/// The binary also searches the built-in Homebrew fallback directories of the
/// host, so the scenario asserts what the fixture alone decides, not the error
/// a host with its own `tailscale` would turn into another one.
fn assert_refused(fixture: &Fixture, path: &str) {
    let listing = fixture.host_list(path);
    let listed = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(
        !listed.contains("alpha"),
        "the refused fake's peer is not listed: {listed}"
    );
    fixture.run(path, &["doctor", "--json"]);
    assert_eq!(
        fixture.fake.invocations(),
        0,
        "the refused fake was never run"
    );
}

/// M1: a user-owned `admin` 0775 directory on `PATH` is trusted.
#[test]
fn an_admin_group_writable_directory_on_path_is_trusted() {
    let fixture = Fixture::new();
    let directory = fixture.directory("m1", ADMIN_GID, 0o775);
    fixture.place_fake(&directory, 0o755);
    assert_resolved(&fixture, &format!("{}:{SYSTEM_PATH}", directory.display()));
}

/// M2: the same directory found through the built-in fallback table.
#[test]
fn an_admin_group_writable_fallback_directory_is_trusted() {
    let fixture = Fixture::new();
    let local = fixture.env.home().join(".local");
    fs::create_dir_all(&local).expect("create ~/.local");
    let directory = fixture.directory("m2-staging", ADMIN_GID, 0o775);
    fs::rename(&directory, local.join("bin")).expect("move into ~/.local/bin");
    let directory = local.join("bin");
    fixture.place_fake(&directory, 0o755);
    assert_resolved(&fixture, SYSTEM_PATH);
}

/// M3: the Homebrew shape, a symlink from `bin` into a keg below `Cellar`.
#[test]
fn a_homebrew_prefix_with_admin_group_directories_is_trusted() {
    let fixture = Fixture::new();
    let prefix = fixture.directory("m3", STAFF_GID, 0o755);
    let bin = prefix.join("bin");
    let cellar = prefix.join("Cellar");
    for directory in [&bin, &cellar] {
        fs::create_dir(directory).expect("create the Homebrew directory");
        set_group_and_mode(directory, ADMIN_GID, 0o775);
    }
    let keg_bin = cellar.join("tailscale/1.104.2/bin");
    fs::create_dir_all(&keg_bin).expect("create the keg");
    let mut keg = cellar.join("tailscale");
    for part in ["1.104.2", "bin"] {
        keg.push(part);
    }
    for directory in [
        cellar.join("tailscale"),
        cellar.join("tailscale/1.104.2"),
        keg,
    ] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("make the keg directory private to writes");
    }
    fixture.place_fake(&keg_bin, 0o555);
    symlink(
        "../Cellar/tailscale/1.104.2/bin/tailscale",
        bin.join("tailscale"),
    )
    .expect("link the keg binary");
    assert_resolved(&fixture, &format!("{}:{SYSTEM_PATH}", bin.display()));
}

/// M4: a group other than `admin` keeps the directory untrusted.
#[test]
fn a_group_writable_directory_of_another_group_is_refused() {
    let fixture = Fixture::new();
    let directory = fixture.directory("m4", STAFF_GID, 0o775);
    fixture.place_fake(&directory, 0o755);
    assert_refused(&fixture, &format!("{}:{SYSTEM_PATH}", directory.display()));
}

/// M5: world write keeps the directory untrusted whatever its group.
#[test]
fn a_world_writable_admin_directory_is_refused() {
    let fixture = Fixture::new();
    let directory = fixture.directory("m5", ADMIN_GID, 0o777);
    fixture.place_fake(&directory, 0o755);
    assert_refused(&fixture, &format!("{}:{SYSTEM_PATH}", directory.display()));
}

/// M6: the executable file check stays strict inside an admin directory.
#[test]
fn a_group_writable_executable_in_an_admin_directory_is_refused() {
    let fixture = Fixture::new();
    let directory = fixture.directory("m6", ADMIN_GID, 0o775);
    let fake = fixture.place_fake(&directory, 0o775);
    set_group_and_mode(&fake, ADMIN_GID, 0o775);
    assert_refused(&fixture, &format!("{}:{SYSTEM_PATH}", directory.display()));
}
