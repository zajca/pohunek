//! Public-API tests for loading, validating, writing, and verifying `service.toml`.

// Rust guideline compliant 2026-09-24

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_platform::filesystem::FsError;
use pohunek_platform::supervisor::Namespace;
use pohunek_service_config::{
    ConfigError, ConfigSpec, Deadlines, ServiceConfig, MAX_ALLOWLIST_ENTRIES, MAX_CONFIG_BYTES,
    MAX_DEADLINE, MAX_PATTERN_BYTES,
};
use proptest::prelude::*;
use tempfile::TempDir;

/// The schema example from the crate contract.
const GOLDEN: &str = r#"schema_version = 1
prefix = "/home/u/.local"
active_version = "0.31.6"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
allowlist = ["PATH","HOME","USER","LOGNAME","SHELL","LANG","LC_*","TMPDIR",
  "SSH_AUTH_SOCK","DISPLAY","WAYLAND_DISPLAY","DBUS_SESSION_BUS_ADDRESS","XDG_*"]

[sweep]
grace_ms = 5000

[limits]
open_files = 8192
"#;

/// Every required key and the `namespace` table, as a missing-field diagnostic names them.
const REQUIRED_KEYS: [&str; 16] = [
    "schema_version",
    "prefix",
    "active_version",
    "uid",
    "state_root",
    "runtime_root",
    "worker_connect_ms",
    "worker_initialize_ms",
    "launchctl_command_ms",
    "worker_exit_timeout_ms",
    "daemon_exit_timeout_ms",
    "daemon_restart_throttle_ms",
    "allowlist",
    "grace_ms",
    "open_files",
    "namespace",
];

/// A `0700` config directory inside a canonical temporary root.
struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temp dir");
        let root = fs::canonicalize(temp.path()).expect("canonicalize temp dir");
        let config_dir = root.join("config");
        fs::create_dir(&config_dir).expect("create config dir");
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
            .expect("chmod config dir");
        Self {
            _temp: temp,
            root,
            config_dir,
        }
    }

    fn path(&self) -> PathBuf {
        self.config_dir.join("service.toml")
    }

    /// Writes `text` as the config file with `mode` and returns its path.
    fn write(&self, text: &str, mode: u32) -> PathBuf {
        let path = self.path();
        // A previous case may have left a read-only file behind.
        if path.exists() {
            fs::remove_file(&path).expect("remove previous config");
        }
        fs::write(&path, text).expect("write config");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod config");
        path
    }

    fn load(&self, text: &str) -> Result<ServiceConfig, ConfigError> {
        ServiceConfig::load(&self.write(text, 0o600))
    }

    /// Creates a directory below the fixture root and returns its canonical path.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).expect("create dir");
        fs::canonicalize(path).expect("canonicalize dir")
    }
}

fn replace(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "fixture lacks {from:?}");
    text.replacen(from, to, 1)
}

fn spec() -> ConfigSpec {
    ConfigSpec {
        prefix: PathBuf::from("/home/u/.local"),
        active_version: "0.31.6".to_owned(),
        uid: 1000,
        state_root: PathBuf::from("/home/u/.local/state/pohunek"),
        runtime_root: PathBuf::from("/run/user/1000/pohunek"),
        deadlines: Deadlines {
            worker_connect: Duration::from_secs(10),
            worker_initialize: Duration::from_secs(45),
            launchctl_command: Duration::from_secs(10),
            worker_exit_timeout: Duration::from_secs(30),
            daemon_exit_timeout: Duration::from_secs(30),
            daemon_restart_throttle: Duration::from_secs(5),
        },
        environment_allowlist: vec!["PATH".to_owned(), "LC_*".to_owned()],
        sweep_grace: Duration::from_secs(5),
        open_files: 8192,
    }
}

fn parse_message(result: Result<ServiceConfig, ConfigError>) -> String {
    match result {
        Err(ConfigError::Parse { message, .. }) => message,
        other => panic!("expected a parse error, got {other:?}"),
    }
}

#[test]
fn golden_example_parses_into_typed_values() {
    let fixture = Fixture::new();
    let config = fixture.load(GOLDEN).expect("golden config loads");

    assert_eq!(config.prefix(), Path::new("/home/u/.local"));
    assert_eq!(config.active_version(), "0.31.6");
    assert_eq!(
        config.version_dir(),
        Path::new("/home/u/.local/libexec/pohunek/0.31.6")
    );
    assert_eq!(
        config.daemon_executable(),
        Path::new("/home/u/.local/libexec/pohunek/0.31.6/pohunekd")
    );
    assert_eq!(
        config.worker_executable(),
        Path::new("/home/u/.local/libexec/pohunek/0.31.6/pohunek-sessiond")
    );
    assert_eq!(config.layout().bin_dir(), Path::new("/home/u/.local/bin"));
    assert_eq!(config.uid(), 1000);
    assert_eq!(
        config.state_root(),
        Path::new("/home/u/.local/state/pohunek")
    );
    assert_eq!(config.runtime_root(), Path::new("/run/user/1000/pohunek"));
    assert_eq!(config.deadlines(), spec().deadlines);
    assert_eq!(config.environment_allowlist().len(), 13);
    assert_eq!(config.environment_allowlist()[6], "LC_*");
    assert_eq!(config.sweep_grace(), Duration::from_secs(5));
    assert_eq!(config.open_files(), 8192);
    assert_eq!(
        config.namespace(),
        Namespace::derive(
            1000,
            Path::new("/home/u/.local/state/pohunek"),
            Path::new("/run/user/1000/pohunek"),
        )
    );
}

#[test]
fn every_missing_key_is_named() {
    let fixture = Fixture::new();
    for key in REQUIRED_KEYS {
        let text = if key == "namespace" {
            let start = GOLDEN.find("[namespace]").expect("namespace table");
            let end = GOLDEN.find("[deadlines]").expect("deadlines table");
            format!("{}{}", &GOLDEN[..start], &GOLDEN[end..])
        } else {
            let start = GOLDEN
                .find(&format!("\n{key} = "))
                .map(|index| index + 1)
                .or_else(|| GOLDEN.starts_with(&format!("{key} = ")).then_some(0))
                .expect("key line");
            let end = if key == "allowlist" {
                GOLDEN.find("\n\n[sweep]").expect("allowlist end") + 1
            } else {
                start + GOLDEN[start..].find('\n').expect("line end") + 1
            };
            format!("{}{}", &GOLDEN[..start], &GOLDEN[end..])
        };
        let message = parse_message(fixture.load(&text));
        assert!(
            message.contains(&format!("missing field `{key}`")),
            "{key}: {message}"
        );
    }
}

#[test]
fn missing_tables_are_named() {
    let fixture = Fixture::new();
    for (table, next) in [
        ("[deadlines]", "[environment]"),
        ("[environment]", "[sweep]"),
        ("[sweep]", "[limits]"),
    ] {
        let start = GOLDEN.find(table).expect("table");
        let end = GOLDEN.find(next).expect("next table");
        let text = format!("{}{}", &GOLDEN[..start], &GOLDEN[end..]);
        let name = table.trim_matches(['[', ']']);
        let message = parse_message(fixture.load(&text));
        assert!(
            message.contains(&format!("missing field `{name}`")),
            "{message}"
        );
    }
    let start = GOLDEN.find("[limits]").expect("limits table");
    let message = parse_message(fixture.load(&GOLDEN[..start]));
    assert!(message.contains("missing field `limits`"), "{message}");
}

#[test]
fn unknown_keys_are_rejected_at_every_level() {
    let fixture = Fixture::new();
    for text in [
        replace(GOLDEN, "prefix =", "surprise = 1\nprefix ="),
        replace(GOLDEN, "uid = 1000", "uid = 1000\nsurprise = 1"),
        replace(GOLDEN, "grace_ms = 5000", "grace_ms = 5000\nsurprise = 1"),
        replace(
            GOLDEN,
            "open_files = 8192",
            "open_files = 8192\nsurprise = 1",
        ),
        format!("{GOLDEN}\n[surprise]\nvalue = 1\n"),
    ] {
        let message = parse_message(fixture.load(&text));
        assert!(message.contains("unknown field `surprise`"), "{message}");
    }
}

#[test]
fn mistyped_values_are_parse_errors() {
    let fixture = Fixture::new();
    for text in [
        replace(GOLDEN, "uid = 1000", "uid = -1"),
        replace(GOLDEN, "uid = 1000", "uid = \"1000\""),
        replace(GOLDEN, "grace_ms = 5000", "grace_ms = 5.0"),
        replace(GOLDEN, "open_files = 8192", "open_files = -8192"),
        "not toml at all".to_owned(),
    ] {
        parse_message(fixture.load(&text));
    }
}

#[test]
fn other_schema_versions_are_rejected_before_their_keys() {
    let fixture = Fixture::new();
    let newer = replace(
        GOLDEN,
        "schema_version = 1",
        "schema_version = 2\nfuture_key = true",
    );
    assert!(matches!(
        fixture.load(&newer),
        Err(ConfigError::UnsupportedSchema { found: 2 })
    ));
    let zero = replace(GOLDEN, "schema_version = 1", "schema_version = 0");
    assert!(matches!(
        fixture.load(&zero),
        Err(ConfigError::UnsupportedSchema { found: 0 })
    ));
}

#[test]
fn durations_are_bounded() {
    let fixture = Fixture::new();
    let max = MAX_DEADLINE.as_millis();
    for (line, key) in [
        ("worker_connect_ms = 10000", "deadlines.worker_connect_ms"),
        (
            "worker_initialize_ms = 45000",
            "deadlines.worker_initialize_ms",
        ),
        (
            "launchctl_command_ms = 10000",
            "deadlines.launchctl_command_ms",
        ),
        (
            "worker_exit_timeout_ms = 30000",
            "deadlines.worker_exit_timeout_ms",
        ),
        (
            "daemon_exit_timeout_ms = 30000",
            "deadlines.daemon_exit_timeout_ms",
        ),
        (
            "daemon_restart_throttle_ms = 5000",
            "deadlines.daemon_restart_throttle_ms",
        ),
        ("grace_ms = 5000", "sweep.grace_ms"),
    ] {
        let name = line.split(" = ").next().expect("key");
        for value in [0, max + 1] {
            let text = replace(GOLDEN, line, &format!("{name} = {value}"));
            match fixture.load(&text) {
                Err(ConfigError::OutOfRange {
                    key: rejected,
                    min: 1,
                    ..
                }) => assert_eq!(rejected, key),
                other => panic!("{key} = {value}: {other:?}"),
            }
        }
        let text = replace(GOLDEN, line, &format!("{name} = {max}"));
        fixture.load(&text).expect("maximum deadline is accepted");
    }
}

#[test]
fn open_files_is_bounded() {
    let fixture = Fixture::new();
    for value in [0, 255, 1_048_577] {
        let text = replace(
            GOLDEN,
            "open_files = 8192",
            &format!("open_files = {value}"),
        );
        assert!(
            matches!(
                fixture.load(&text),
                Err(ConfigError::OutOfRange {
                    key: "limits.open_files",
                    min: 256,
                    max: 1_048_576,
                    ..
                })
            ),
            "{value}"
        );
    }
    for value in [256, 1_048_576] {
        let text = replace(
            GOLDEN,
            "open_files = 8192",
            &format!("open_files = {value}"),
        );
        fixture.load(&text).expect("bound is inclusive");
    }
}

#[test]
fn sub_millisecond_durations_cannot_be_recorded() {
    let mut value = spec();
    value.deadlines.worker_initialize = Duration::from_micros(1_500);
    assert!(matches!(
        ServiceConfig::new(value),
        Err(ConfigError::Precision {
            key: "deadlines.worker_initialize_ms"
        })
    ));
    let mut value = spec();
    value.sweep_grace = Duration::from_nanos(1);
    assert!(matches!(
        ServiceConfig::new(value),
        Err(ConfigError::Precision {
            key: "sweep.grace_ms"
        })
    ));
}

#[test]
fn paths_must_be_absolute_and_normalized() {
    let fixture = Fixture::new();
    for (line, key) in [
        ("prefix = \"/home/u/.local\"", "prefix"),
        (
            "state_root = \"/home/u/.local/state/pohunek\"",
            "namespace.state_root",
        ),
        (
            "runtime_root = \"/run/user/1000/pohunek\"",
            "namespace.runtime_root",
        ),
    ] {
        let name = line.split(" = ").next().expect("key");
        for bad in [
            "relative/dir",
            "",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/b/",
            "./a",
            "/a/\\u0000b",
        ] {
            let text = replace(GOLDEN, line, &format!("{name} = \"{bad}\""));
            match fixture.load(&text) {
                Err(ConfigError::InvalidPath { key: rejected, .. }) => {
                    assert_eq!(rejected, key, "{bad}");
                }
                other => panic!("{key} = {bad:?}: {other:?}"),
            }
        }
        let long = format!("/{}", "a".repeat(4096));
        let text = replace(GOLDEN, line, &format!("{name} = \"{long}\""));
        assert!(matches!(
            fixture.load(&text),
            Err(ConfigError::InvalidPath { .. })
        ));
    }
}

#[test]
fn install_versions_must_be_one_safe_component() {
    let fixture = Fixture::new();
    let too_long = "1".repeat(65);
    for bad in [
        "",
        ".",
        "..",
        "../x",
        "a/b",
        "0.31 6",
        "v:1",
        too_long.as_str(),
    ] {
        let text = replace(
            GOLDEN,
            "active_version = \"0.31.6\"",
            &format!("active_version = \"{bad}\""),
        );
        match fixture.load(&text) {
            Err(ConfigError::InvalidVersion { value }) => assert_eq!(value, bad),
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    let text = replace(
        GOLDEN,
        "active_version = \"0.31.6\"",
        "active_version = \"0.32.0-rc.1+build.7\"",
    );
    fixture
        .load(&text)
        .expect("semver-like version is accepted");
}

#[test]
fn reserved_uid_is_rejected() {
    let fixture = Fixture::new();
    let text = replace(GOLDEN, "uid = 1000", "uid = 4294967295");
    assert!(matches!(
        fixture.load(&text),
        Err(ConfigError::InvalidUid { uid: u32::MAX })
    ));
}

#[test]
fn allowlist_patterns_are_names_or_trailing_prefixes() {
    let fixture = Fixture::new();
    let with_allowlist = |entries: &[&str]| {
        let rendered: Vec<String> = entries.iter().map(|entry| format!("{entry:?}")).collect();
        let start = GOLDEN.find("allowlist = ").expect("allowlist");
        let end = GOLDEN.find("\n\n[sweep]").expect("allowlist end");
        format!(
            "{}allowlist = [{}]{}",
            &GOLDEN[..start],
            rendered.join(", "),
            &GOLDEN[end..]
        )
    };

    let long_pattern = "A".repeat(MAX_PATTERN_BYTES + 1);
    for bad in [
        "*",
        "",
        "1PATH",
        "PA-TH",
        "PA*TH",
        "PATH**",
        "PÄTH",
        " PATH",
        "PATH=1",
        long_pattern.as_str(),
    ] {
        match fixture.load(&with_allowlist(&["HOME", bad])) {
            Err(ConfigError::InvalidPattern { pattern, .. }) => assert_eq!(pattern, bad),
            other => panic!("{bad:?}: {other:?}"),
        }
    }

    assert!(matches!(
        fixture.load(&with_allowlist(&[])),
        Err(ConfigError::AllowlistSize { count: 0, .. })
    ));
    let many: Vec<String> = (0..=MAX_ALLOWLIST_ENTRIES)
        .map(|index| format!("VAR_{index}"))
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(matches!(
        fixture.load(&with_allowlist(&many)),
        Err(ConfigError::AllowlistSize { count, .. }) if count == MAX_ALLOWLIST_ENTRIES + 1
    ));
    assert!(matches!(
        fixture.load(&with_allowlist(&["PATH", "HOME", "PATH"])),
        Err(ConfigError::DuplicatePattern { pattern }) if pattern == "PATH"
    ));

    let longest = "A".repeat(MAX_PATTERN_BYTES - 1) + "*";
    let config = fixture
        .load(&with_allowlist(&[
            "_",
            "__CFBundleIdentifier",
            "LC_*",
            "X*",
            &longest,
        ]))
        .expect("valid patterns are accepted");
    assert_eq!(config.environment_allowlist()[4], longest);
    let maximum = &many[..MAX_ALLOWLIST_ENTRIES];
    fixture
        .load(&with_allowlist(maximum))
        .expect("the maximum entry count is accepted");
}

#[test]
fn group_or_world_accessible_files_are_rejected() {
    let fixture = Fixture::new();
    for mode in [0o644, 0o640, 0o604, 0o660, 0o400, 0o700] {
        let path = fixture.write(GOLDEN, mode);
        assert!(
            matches!(
                ServiceConfig::load(&path),
                Err(ConfigError::Untrusted { .. })
            ),
            "{mode:o}"
        );
    }
}

#[test]
fn readable_owner_directories_are_accepted() {
    let fixture = Fixture::new();
    let path = fixture.write(GOLDEN, 0o600);
    for mode in [0o700, 0o750, 0o755] {
        fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(mode))
            .expect("chmod config dir");
        ServiceConfig::load(&path).unwrap_or_else(|error| panic!("{mode:o}: {error}"));
    }
}

#[test]
fn writable_by_others_directories_are_rejected() {
    let fixture = Fixture::new();
    let path = fixture.write(GOLDEN, 0o600);
    for mode in [0o775, 0o777, 0o757, 0o1777] {
        fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(mode))
            .expect("chmod config dir");
        let result = ServiceConfig::load(&path);
        assert!(
            matches!(result, Err(ConfigError::Untrusted { .. })),
            "{mode:o}: {result:?}"
        );
    }
}

/// Returns whether the tests run as root, which owns the system directories.
fn running_as_root() -> bool {
    let probe = tempfile::tempdir().expect("create temp dir");
    fs::metadata(probe.path()).expect("stat temp dir").uid() == 0
}

#[test]
fn foreign_owned_directories_are_rejected() {
    // `/usr` is a real, root-owned, not world-writable directory on Linux and
    // Darwin, so it is foreign-owned for any unprivileged user. A test run as
    // root cannot produce a foreign owner without `chown`, so it skips.
    if running_as_root() {
        return;
    }
    let path = Path::new("/usr/service.toml");
    let result = ServiceConfig::load(path);
    assert!(
        matches!(
            result,
            Err(ConfigError::Untrusted {
                source: FsError::UnsafeOwner { .. },
                ..
            })
        ),
        "{result:?}"
    );
    let config = ServiceConfig::new(spec()).expect("valid spec");
    let result = config.write(path);
    assert!(
        matches!(
            result,
            Err(ConfigError::Untrusted {
                source: FsError::UnsafeOwner { .. },
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn symlinks_are_rejected() {
    let fixture = Fixture::new();
    let target = fixture.config_dir.join("real.toml");
    fs::write(&target, GOLDEN).expect("write target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod target");
    symlink(&target, fixture.path()).expect("create symlink");
    assert!(matches!(
        ServiceConfig::load(&fixture.path()),
        Err(ConfigError::Untrusted { .. })
    ));

    let linked_dir = fixture.root.join("linked");
    symlink(&fixture.config_dir, &linked_dir).expect("create dir symlink");
    fs::remove_file(fixture.path()).expect("remove file symlink");
    fixture.write(GOLDEN, 0o600);
    // The no-follow directory open fails with an OS error (ENOTDIR on Linux,
    // ELOOP on Darwin) rather than a typed trust violation; either way the
    // symlinked directory is never traversed.
    let result = ServiceConfig::load(&linked_dir.join("service.toml"));
    assert!(
        matches!(
            result,
            Err(ConfigError::Io { .. } | ConfigError::Untrusted { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn hard_links_are_rejected() {
    let fixture = Fixture::new();
    let path = fixture.write(GOLDEN, 0o600);
    fs::hard_link(&path, fixture.config_dir.join("alias.toml")).expect("create hard link");
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::Untrusted { .. })
    ));
}

#[test]
fn oversized_files_are_rejected() {
    let fixture = Fixture::new();
    let padding = format!("# {}\n", "x".repeat(MAX_CONFIG_BYTES));
    assert!(matches!(
        fixture.load(&format!("{padding}{GOLDEN}")),
        Err(ConfigError::TooLarge { max_bytes, .. }) if max_bytes == MAX_CONFIG_BYTES
    ));
}

#[test]
fn non_utf8_files_are_parse_errors() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let mut bytes = GOLDEN.as_bytes().to_vec();
    bytes.extend_from_slice(b"# \xff\n");
    fs::write(&path, bytes).expect("write config");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod config");
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn config_paths_must_be_absolute_and_present() {
    let fixture = Fixture::new();
    for bad in ["service.toml", "/", "/a/../service.toml", "/a/b/"] {
        assert!(
            matches!(
                ServiceConfig::load(Path::new(bad)),
                Err(ConfigError::ConfigPath { .. })
            ),
            "{bad}"
        );
    }
    match ServiceConfig::load(&fixture.path()) {
        Err(ConfigError::Io { source, .. }) => {
            assert_eq!(source.io_kind(), Some(std::io::ErrorKind::NotFound));
        }
        other => panic!("missing file: {other:?}"),
    }
}

#[test]
fn write_creates_a_private_file_in_a_private_directory() {
    let fixture = Fixture::new();
    let config = ServiceConfig::new(spec()).expect("valid spec");
    let path = fixture
        .root
        .join("fresh")
        .join("pohunek")
        .join("service.toml");
    config.write(&path).expect("write config");

    let file_mode = fs::metadata(&path).expect("stat file").permissions().mode() & 0o7777;
    let dir_mode = fs::metadata(path.parent().expect("parent"))
        .expect("stat dir")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(file_mode, 0o600);
    assert_eq!(dir_mode, 0o700);
    assert_eq!(ServiceConfig::load(&path).expect("reload"), config);

    let mut upgraded = config.to_spec();
    upgraded.active_version = "0.32.0".to_owned();
    let upgraded = ServiceConfig::new(upgraded).expect("valid upgrade");
    upgraded.write(&path).expect("replace config");
    assert_eq!(ServiceConfig::load(&path).expect("reload"), upgraded);
    let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
        .expect("list dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(leftovers, ["service.toml"]);
}

#[test]
fn write_keeps_an_existing_readable_directory_mode() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(0o755))
        .expect("chmod config dir");
    let config = ServiceConfig::new(spec()).expect("valid spec");
    config.write(&fixture.path()).expect("write config");
    let dir_mode = fs::metadata(&fixture.config_dir)
        .expect("stat dir")
        .permissions()
        .mode()
        & 0o7777;
    let file_mode = fs::metadata(fixture.path())
        .expect("stat file")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(dir_mode, 0o755);
    assert_eq!(file_mode, 0o600);
    assert_eq!(
        ServiceConfig::load(&fixture.path()).expect("reload"),
        config
    );
}

#[test]
fn write_refuses_a_directory_writable_by_others() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(0o775))
        .expect("chmod config dir");
    let config = ServiceConfig::new(spec()).expect("valid spec");
    assert!(matches!(
        config.write(&fixture.path()),
        Err(ConfigError::Untrusted { .. })
    ));
    assert!(!fixture.path().exists());
}

#[test]
fn namespace_uses_the_recorded_canonical_roots() {
    let fixture = Fixture::new();
    let state = fixture.dir("state/pohunek");
    let runtime = fixture.dir("runtime/pohunek");
    let mut value = spec();
    value.state_root.clone_from(&state);
    value.runtime_root.clone_from(&runtime);
    let config = ServiceConfig::new(value).expect("valid spec");

    assert_eq!(
        config.namespace(),
        Namespace::derive(
            1000,
            &fs::canonicalize(&state).expect("canonical state"),
            &fs::canonicalize(&runtime).expect("canonical runtime"),
        )
    );
    config
        .verify_installation(1000, &state, &runtime)
        .expect("same installation verifies");

    // An equivalent spelling through a symlink canonicalizes to the same root.
    let alias = fixture.root.join("state-alias");
    symlink(&state, &alias).expect("create alias");
    config
        .verify_installation(1000, &alias, &runtime)
        .expect("aliased root verifies");
}

#[test]
fn verify_installation_rejects_another_installation() {
    let fixture = Fixture::new();
    let state = fixture.dir("state/pohunek");
    let runtime = fixture.dir("runtime/pohunek");
    let other = fixture.dir("other/pohunek");
    let mut value = spec();
    value.state_root.clone_from(&state);
    value.runtime_root.clone_from(&runtime);
    let config = ServiceConfig::new(value).expect("valid spec");

    assert!(matches!(
        config.verify_installation(1001, &state, &runtime),
        Err(ConfigError::NamespaceMismatch {
            key: "namespace.uid",
            ..
        })
    ));
    match config.verify_installation(1000, &other, &runtime) {
        Err(ConfigError::NamespaceMismatch {
            key: "namespace.state_root",
            recorded,
            actual,
        }) => {
            assert_eq!(recorded, state.display().to_string());
            assert_eq!(actual, other.display().to_string());
        }
        result => panic!("state mismatch: {result:?}"),
    }
    assert!(matches!(
        config.verify_installation(1000, &state, &other),
        Err(ConfigError::NamespaceMismatch {
            key: "namespace.runtime_root",
            ..
        })
    ));
    assert!(matches!(
        config.verify_installation(1000, &state, &fixture.root.join("missing")),
        Err(ConfigError::Canonicalize {
            key: "namespace.runtime_root",
            ..
        })
    ));
}

#[test]
fn errors_name_the_rejected_key() {
    let fixture = Fixture::new();
    let text = replace(GOLDEN, "grace_ms = 5000", "grace_ms = 0");
    let error = fixture.load(&text).expect_err("zero grace is rejected");
    assert_eq!(
        error.to_string(),
        "service config key sweep.grace_ms = 0 is outside 1..=600000"
    );
}

/// Generates valid specs spanning every bound.
fn arb_spec() -> impl Strategy<Value = ConfigSpec> {
    let max = u64::try_from(MAX_DEADLINE.as_millis()).expect("bounded");
    let millis = move || (1..=max).prop_map(Duration::from_millis);
    let path = || {
        prop::collection::vec("[a-zA-Z0-9._-]{1,12}", 1..6).prop_filter_map(
            "no dot components",
            |parts| {
                parts
                    .iter()
                    .all(|part| part != "." && part != "..")
                    .then(|| PathBuf::from(format!("/{}", parts.join("/"))))
            },
        )
    };
    let pattern = "[A-Za-z_][A-Za-z0-9_]{0,20}\\*?";
    (
        (
            path(),
            "[0-9A-Za-z+-][0-9A-Za-z.+-]{0,40}",
            0..u32::MAX,
            path(),
            path(),
        ),
        (millis(), millis(), millis(), millis(), millis(), millis()),
        prop::collection::btree_set(pattern, 1..40),
        millis(),
        256..=1_048_576_u64,
    )
        .prop_map(
            |(
                (prefix, active_version, uid, state_root, runtime_root),
                (connect, initialize, launchctl, worker_exit, daemon_exit, throttle),
                allowlist,
                sweep_grace,
                open_files,
            )| ConfigSpec {
                prefix,
                active_version,
                uid,
                state_root,
                runtime_root,
                deadlines: Deadlines {
                    worker_connect: connect,
                    worker_initialize: initialize,
                    launchctl_command: launchctl,
                    worker_exit_timeout: worker_exit,
                    daemon_exit_timeout: daemon_exit,
                    daemon_restart_throttle: throttle,
                },
                environment_allowlist: allowlist.into_iter().collect(),
                sweep_grace,
                open_files,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn write_then_load_round_trips(value in arb_spec()) {
        let fixture = Fixture::new();
        let config = ServiceConfig::new(value.clone()).expect("generated spec is valid");
        prop_assert_eq!(config.to_spec(), value);
        config.write(&fixture.path()).expect("write config");
        let loaded = ServiceConfig::load(&fixture.path()).expect("load config");
        prop_assert_eq!(&loaded, &config);
        prop_assert_eq!(loaded.to_toml(), config.to_toml());
    }
}
